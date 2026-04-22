# Docker inside the VM

There are two ways to get Docker working inside an airlock sandbox:
forwarding the host's Docker socket (quick, but with security trade-offs)
or running a Docker engine inside the VM itself (isolated, but more setup).

## Option 1: Forward the host Docker socket

The simplest approach is to install only the Docker CLI inside the VM and
forward the host's Docker socket through airlock's socket forwarding.

On Linux, the socket is typically at `/var/run/docker.sock`:

```toml
# airlock.toml
[network.sockets.docker]
host = "/var/run/docker.sock"
```

On macOS with Docker Desktop, the socket lives in the user's home directory,
so use `source:target` syntax to map it to the standard guest path:

```toml
# airlock.local.toml
[network.sockets.docker]
host = "~/.docker/run/docker.sock:/var/run/docker.sock"
```

The guest path stays the same in both cases — the Docker CLI inside the VM
looks for `/var/run/docker.sock` regardless of where the host socket is.

With this setup, `docker build`, `docker run`, and other commands inside the
sandbox talk to the host Docker daemon. There is nothing else to configure —
the socket relay is transparent.

**Security note:** this gives processes inside the sandbox full access to
the host Docker daemon. A sandboxed process could mount host directories,
access host networking, or run privileged containers — effectively escaping
the sandbox. This is fine for trusted development workflows but defeats the
isolation guarantees if you're sandboxing untrusted code.

## Option 2: Run Docker engine inside the VM

For full isolation, you can run `dockerd` inside the VM. The airlock kernel
ships with all the necessary support — cgroups v2, overlayfs, netfilter,
namespaces, seccomp — so Docker works out of the box.

There are two things to set up: storage and the daemon process.

### Storage

Docker's overlayfs snapshotter cannot run on top of the VM's own overlayfs
root filesystem. It needs a regular filesystem, and airlock provides one at
`/airlock/disk` — a persistent ext4 mount backed by the project's disk image.

Before starting Docker, bind-mount a subdirectory of `/airlock/disk` to
`/var/lib/docker`:

```bash
mkdir -p /airlock/disk/docker /var/lib/docker
mount --bind /airlock/disk/docker /var/lib/docker
```

The data in `/airlock/disk` persists across sandbox restarts, so your Docker
images and build cache survive reboots.

### Starting the daemon

airlock's VM does not run systemd or any other init system beyond the
lightweight `airlockd` supervisor. This means `dockerd` won't start
automatically — you need to launch it yourself.

The simplest approach is to start it in the background before running your
actual command:

```bash
dockerd &>/var/log/dockerd.log &
sleep 2  # wait for socket
docker run hello-world
```

For a more structured setup, you can write a small wrapper script that
starts `dockerd` and waits for the socket to appear:

```bash
#!/bin/bash
# start-docker.sh
mkdir -p /airlock/disk/docker /var/lib/docker
mount --bind /airlock/disk/docker /var/lib/docker

dockerd &>/var/log/dockerd.log &
DOCKERD_PID=$!

# Wait for the socket
for i in $(seq 1 30); do
    [ -S /var/run/docker.sock ] && break
    sleep 0.5
done

if [ ! -S /var/run/docker.sock ]; then
    echo "dockerd failed to start" >&2
    exit 1
fi

exec "$@"
```

Then use it as your sandbox entry point:

```bash
airlock start -- ./start-docker.sh bash
```

### Disabling security hardening

If Docker commands fail with permission errors, you may need to disable
airlock's security hardening, which restricts namespace creation and
applies `no-new-privileges`:

```toml
[vm]
harden = false
```

This gives processes inside the container the full set of kernel
capabilities they need to create namespaces and manage cgroups.

### Container networking

Container egress works on the default Compose bridge — no `network_mode:
host` workarounds needed. airlock's TCP proxy runs on a TUN device
(`airlock0`) wired as the VM's default route, so every outbound packet
ends up in the proxy regardless of which netns it came from. Docker's
own bridge + MASQUERADE rules pass traffic through unchanged; the
proxy just sees MASQUERADE'd source IPs (the VM's airlock0 address),
which is fine because airlock keys policy on the *destination*.

Compose's service-name DNS works as expected (it's internal to the
bridge network and never touches airlock's virtual DNS). Published
ports (`ports: ["8000:8000"]`) also work via the standard loopback
path: docker-proxy binds the VM's `127.0.0.1:<port>`, and airlock's
`guest = [8000]` reverse-forward exposes that on the host.
