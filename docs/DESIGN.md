# Design

## Overview

airlock runs untrusted code inside a lightweight Linux VM. A single `airlock`
binary boots a VM, pulls an OCI container image, assembles an overlayfs
rootfs, and gives the user an interactive shell (or runs a one-off command)
inside the container. The VM provides hardware-level isolation; the container
provides a familiar image-based environment.

```
┌─ HOST (macOS / Linux) ─────────────────────────────────────────┐
│                                                                  │
│  airlock (CLI)                                                   │
│  ├─ Pull + cache OCI image (host-side)                          │
│  ├─ Boot Linux VM (Apple Virtualization / Cloud Hypervisor+KVM) │
│  │   ├─ Kernel + initramfs embedded in binary                   │
│  │   ├─ VirtioFS shares rootfs layers and mounts into VM        │
│  │   └─ vsock connects CLI ↔ supervisor (Cap'n Proto RPC)       │
│  ├─ Start container, relay terminal I/O                         │
│  ├─ Network proxy (allow/deny rules + TLS interception)         │
│  └─ CLI server (Unix socket) — serves airlock exec connections  │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │ VM (Linux, ARM64)                                        │   │
│  │                                                          │   │
│  │  init                                                    │   │
│  │  ├─ Mount VirtioFS shares (image layers, overlay, mounts)│   │
│  │  ├─ Assemble overlayfs rootfs                            │   │
│  │  ├─ Setup networking (iptables + DNS)                    │   │
│  │  └─ Launch supervisor (airlockd)                         │   │
│  │                                                          │   │
│  │  airlockd (supervisor, static musl binary)               │   │
│  │  ├─ Listen on vsock port 1024                            │   │
│  │  ├─ Accept RPC connection from CLI                       │   │
│  │  ├─ Assemble overlayfs, chroot, exec container process   │   │
│  │  └─ Network relay: proxy guest TCP → host NetworkProxy   │   │
│  └──────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────┘
```


## Virtualization

### macOS: Apple Virtualization framework

Uses the native `com.apple.Virtualization` framework via Rust bindings
(`objc2-virtualization`). The binary requires two entitlements and must be
ad-hoc codesigned after every build:

```
com.apple.security.virtualization
com.apple.security.hypervisor
```

### Linux: Cloud Hypervisor + KVM

Uses Cloud Hypervisor with KVM acceleration. The `cloud-hypervisor` and
`virtiofsd` binaries are embedded in the `airlock` binary and extracted on
first run. Requires `/dev/kvm` access; `airlock` checks and reports permission
issues at startup.

### Kernel and initramfs

- **Kernel**: Linux 6.18 built from source with a minimal config (no EFI
  stub — VZLinuxBootLoader and Cloud Hypervisor both require a raw ARM64
  `Image`). Built inside Docker; embedded in the `airlock` binary via
  `include_bytes!`.
- **Initramfs**: Alpine 3.23 with the `airlockd` supervisor binary and a
  minimal init script. Built inside Docker as a gzipped cpio archive;
  also embedded in the binary.
- Both are extracted to `~/.cache/airlock/kernel/` on first run. A checksum
  check re-extracts them if the binary is updated.

### Virtio devices

| Device | Purpose |
|--------|---------|
| Serial console | Kernel debug output |
| Entropy | `/dev/urandom` in guest |
| Memory balloon | Future: reclaim unused guest memory |
| vsock | Host ↔ guest RPC (port 1024) |
| VirtioFS | Shared filesystems (image layers, overlay, mounts) |
| Block (ext4) | Per-project persistent disk |


## Communication: vsock + Cap'n Proto RPC

CLI and supervisor communicate over a single vsock connection (host port 1024
on macOS TCP, Linux vsock) using Cap'n Proto RPC in twoparty transport mode.

**Supervisor listens, CLI connects.** The CLI polls the vsock port after VM
boot until the supervisor is ready. This avoids implementing callback
delegates in the host virtualization API.

### Protocol (supervisor.capnp)

```
Supervisor
  start(stdin, pty, network, logs, logFilter,
        epoch, hostPorts, sockets,
        cmd, args, env, cwd, uid, gid, nestedVirt, harden,
        imageId, dirs, files, caches) → Process
    # Boot-time call. Carries all process config (replaces config.json)
    # and mount config (replaces mounts.json). Triggers VM init, then
    # forks and execs the container process directly (no crun).

  exec(stdin, pty, cmd, args, cwd, env) → Process
    # Attach a new process to the running container.
    # Called once per `airlock exec` invocation.

  shutdown() → ()
    # Sync filesystems before VM teardown.

CliService   (Unix socket: <project>/.airlock/sandbox/cli.sock)
  exec(stdin, pty, cmd, args, cwd, env) → Process
    # Exposed by `airlock up` for `airlock exec` to connect to.

Process
  poll() → (exit:Int32 | stdout:Data | stderr:Data)
  signal(signum) → ()
  kill() → ()

Stdin
  read() → (data:Data | resize:(rows, cols))

NetworkProxy
  connect(target, clientSink) → (serverSink | denied)
    # TCP relay: guest connects to target, host bridges to real host.

LogSink
  log(level, message) → stream
```


## VM init

Inside the VM, the init script mounts essential filesystems (`/proc`, `/sys`,
`/dev`, `/cgroup2`) then launches the supervisor (`airlockd`). The supervisor's
init closure (called on the first `start` RPC) does the heavy setup:

1. **Mount VirtioFS shares** — `layers` (shared per-layer OCI cache at
   `~/.cache/airlock/layers/`, read-only), `ca` (CA cert lowerdir), and
   `files/rw` + `files/ro` (file mount staging) are mounted as needed. The
   full mount list and the ordered image-layer digest list are received via
   the `start` RPC (no separate `mounts.json`).

2. **Set system clock** — host passes a Unix epoch in the start RPC so the
   guest clock is correct from the start.

3. **Setup networking** — loopback IP `10.0.0.1/8`, default route, iptables
   rules for localhost port forwarding (port N → 15001, the in-VM TCP proxy).

4. **Mount the project disk** — formats the ext4 image if blank (`mkfs.ext4`),
   then mounts it at `/mnt/disk`. Resizes the filesystem if the disk image was
   enlarged.

5. **Assemble overlayfs rootfs**:
   - **lower**: CA cert layer (`/mnt/ca`) if present, then one lowerdir per
     image layer at `/mnt/layers/<layer-digest>/rootfs` in topmost-first
     order. Leftmost = highest priority. Mounted with `userxattr` so
     overlayfs honors whiteouts encoded by the host-side extractor as
     `user.overlay.whiteout` / `user.overlay.opaque` xattrs (requires kernel
     >= 5.11).
   - **upper + work**: on the ext4 disk (`/mnt/disk/overlay/rootfs` and
     `/mnt/disk/overlay/work`). The overlay is reset if the image ID changes.
   - After mounting: file mounts become symlinks inside the rootfs pointing
     into `/airlock/.files/rw` or `/airlock/.files/ro`. Directory mounts are
     bind-mounted. Cache directories are bind-mounted last (override dir mounts).

6. **Setup DNS** — writes `nameserver 10.0.0.1` to `/etc/resolv.conf` in the
   rootfs. DNS queries go to the in-VM network proxy, which resolves them on
   the host.

The overlay upper layer on disk means writable container state persists across
runs. A stored image ID (`/mnt/disk/overlay/.image_id`) triggers a full upper
reset when the base image changes.


## Container execution

### Process spawning

The supervisor (`airlockd`) does not use an OCI runtime. After assembling the
overlayfs rootfs, it spawns container processes directly via fork + chroot +
exec:

- **chroot**: into the assembled overlayfs rootfs.
- **uid/gid**: switched to the container user (read from `start` RPC params,
  derived from the image's `/etc/passwd`).
- **PTY**: allocated when stdin is a TTY; host terminal size is sent as the
  initial PTY dimensions, and resize events (SIGWINCH) are forwarded.
- **Pipe mode**: when stdin is not a TTY, separate stdout/stderr pipes are
  used with no PTY.

All process configuration (`cmd`, `args`, `env`, `cwd`, `uid`, `gid`) is
carried in the `start` RPC call rather than written to a `config.json` file.

### `airlock exec` — attach to a running container

`airlock exec` attaches a new process to an already-running container without
rebooting the VM. Flow:

1. `airlock exec` connects to `<project>/.airlock/sandbox/cli.sock` (Unix
   socket, Cap'n Proto RPC) that `airlock up` exposes while the VM is running.
2. The CLI server forwards the exec request to the in-VM supervisor via the
   existing RPC connection (reusing the established vsock).
3. The supervisor forks a new process inside the container's chroot namespace.
4. I/O is relayed back to the `airlock exec` terminal.


## File and directory mounting

### How VirtioFS shares work

Each directory mount becomes a VirtioFS share (one virtio device per share).
The guest init mounts each share under `/mnt/<tag>`. The supervisor then
bind-mounts from `/mnt/<tag>` to the desired container path after chroot.

### Project directory

Always mounted at the same absolute path as on the host. This means paths in
build tools, error messages, and scripts are identical inside and outside the
sandbox. The container shell's working directory is set to this path.

### Directory mounts

A VirtioFS share pointing directly at the host directory. Read-only or
read-write as configured.

### File mounts

VirtioFS does not support file-level bind mounts reliably. Instead:

1. The file is hard-linked into a staging directory (`sandbox/overlay/files/rw/` or
   `sandbox/overlay/files/ro/`) which is the VirtioFS share root.
2. Inside the rootfs, a symlink at the target path points into
   `/airlock/.files/rw` or `/airlock/.files/ro`.

Hard links provide bidirectional sync — changes in the container appear on
the host and vice versa. If hard-linking fails (cross-filesystem), the file
is copied with a warning that sync is one-way.

### CA cert layer

The project CA certificate (used for TLS interception) is installed as an
extra overlayfs lowerdir rather than a symlink or file mount. The cert bundle
is written to `sandbox/ca/` mirroring the distro's CA store paths (e.g.
`sandbox/ca/etc/ssl/certs/ca-certificates.crt`). The supervisor prepends
`/mnt/overlay/ca` as the highest-priority lowerdir:

```
lowerdir=/mnt/ca:/mnt/layers/<top-digest>/rootfs:…:/mnt/layers/<bottom-digest>/rootfs
```

The cert appears as a regular file inside the container, which is required by
curl's `CURLOPT_CAINFO` loading. Symlinks do not work for this purpose.


## Networking

All outbound network access from the container goes through a host-side HTTP
proxy. Inside the VM, iptables redirects all outbound TCP to the supervisor's
proxy listener on `127.0.0.1:15001`. The supervisor relays connections back
to the CLI via the `NetworkProxy.connect()` RPC.

### Allow/deny rules

Rules are evaluated per connection against the resolved hostname and port.
Each rule has an `allow` list and an optional `deny` list. Rules accumulate
additively across config files and presets. `enabled = false` disables a rule
including one inherited from a preset.

Decision logic (for `allow-by-default` and `deny-by-default` policies):
1. If any `deny` pattern matches → **block** immediately (deny wins).
2. If any `allow` pattern matches → **allow**.
3. If neither matched → follow `policy` (`allow-by-default` allows,
   `deny-by-default` denies).

`allow-always` skips rules and allows everything. `deny-always` denies
everything including port forwards and sockets.

Pattern formats (used in both `allow` and `deny` lists):
- `host` — exact hostname, any port
- `host:port` — exact hostname and port
- `*:port` — any hostname on a specific port
- `*` — match all (use only for development)

### TLS interception

Per-project: a self-signed CA certificate and key are generated in the
project cache on first run. The CLI installs the CA cert into the container
rootfs via the overlayfs lowerdir mechanism so containers trust it
automatically.

TLS interception is applied **only** for connections that match a
`[network.middleware]` target pattern. Connections without matching
middleware pass through as raw TCP — no TLS MITM.

### Lua middleware

Middleware scripts run for each intercepted HTTP request/response. Scripts
are compiled to bytecode at startup (zero overhead per request for compilation).

```lua
function modify_request(req)
    req:header("Authorization", "Bearer " .. os.getenv("API_KEY"))
    return req
end
```

### Localhost port forwarding

Ports declared as "host ports" in the config get iptables `REDIRECT` rules
inside the VM so that connections to `127.0.0.1:<port>` are transparently
forwarded to the host.

### Unix socket forwarding

Host Unix sockets are forwarded into the container. When the container connects
to the guest socket path, the supervisor sends the guest path to the CLI via
`NetworkProxy.connect`. The CLI maps guest path → host path using a pre-built
`socket_map` (with tilde expansion applied at setup time) and opens a
connection to the host socket.

`~` in guest paths is expanded to the container home directory (read from the
image's `/etc/passwd`). `~` in host paths is expanded to the host user's home
directory.


## Project management

### Sandbox directory

Each project stores its sandbox state locally in `.airlock/` next to the
config file. A `.gitignore` containing `*` is written there automatically so
nothing under `.airlock/` is tracked by version control.

```
<project>/
  airlock.toml                   # user config (tracked by VCS)
  .airlock/
    .gitignore                   # contains "*" — auto-created
    sandbox/
      ca.json                    # CA cert + key PEMs (JSON, single source of truth)
      ca/                        # overlayfs lowerdir: CA cert injected into container
      │                          #   trust stores (etc/ssl/certs/…, etc/pki/…)
      overlay/
        files/rw/{key}           # hard-linked writable file mounts (VirtioFS share root)
        files/ro/{key}           # hard-linked read-only file mounts (VirtioFS share root)
      disk.img                   # virtio-blk ext4 volume (rootfs overlay upper + caches)
      run.json                   # last_run timestamp, guest_cwd override
      run.log                    # tracing log from last `airlock up`
      lock                       # PID lockfile (one VM per project at a time)
      cli.sock                   # Unix socket for `airlock exec` RPC
      image                      # hard-link to image_cache/<digest> (GC ref + cached image)
```

`airlock down` removes the entire `.airlock/` directory. The config file is
untouched.

### CA keypair

On first `airlock up`, a self-signed CA keypair is generated and written to
`sandbox/ca.json` as a JSON object with `cert` and `key` PEM fields. The PEM
strings are read into memory at startup — no further file reads are needed at
TLS setup time. The CA cert is injected into the container via the `sandbox/ca/`
overlayfs lowerdir so containers trust it automatically.

### Image cache

Images are cached at `~/.cache/airlock/oci/`, split into per-image
metadata and a shared per-layer extraction cache:

```
~/.cache/airlock/
  kernel/
    Image                    # Linux kernel (extracted from binary on first run)
    initramfs.gz             # initramfs
    cloud-hypervisor         # (Linux only) hypervisor binary
    virtiofsd                # (Linux only) VirtioFS daemon
    checksum                 # triggers re-extraction when binary is updated
  oci/
    images/<digest>            # Schema-tagged JSON: the fully-baked `OciImage`
                               # (name, layers, uid/gid, cmd, env, container_home)
    layers/<digest>/
      rootfs/                      # Extracted layer tree (whiteouts as xattrs)
    layers/<digest>.download.tmp   # In-flight download (swept on next run)
    layers/<digest>.download       # Complete tarball pending extraction
    layers/<digest>.tmp/           # In-flight extraction (swept on next run)
```

The image cache is shared across all projects. Layers are
content-addressable by digest, so two images that share a base layer
extract it only once. There is no merged per-image rootfs — the guest
composes overlayfs directly from the per-layer trees. Platform is
fixed to `linux/arm64` (matching the VM architecture).

Each `images/<digest>` entry is a single JSON file carrying the
serialized `OciImage` (wrapped in a `{"schema":"v1", …}` envelope for
forward-compatible schema evolution). It is written atomically via
`.tmp` rename and then hard-linked to `sandbox/image`; a link count
greater than 1 on the file means at least one sandbox references the
image, preventing GC.

A `<digest>/rootfs/` directory only exists through the atomic rename
from `<digest>.tmp/`, so its presence is itself the completion marker
— no separate `.ok` file is needed.

`sandbox/image` is the per-project GC ref and also the stored-image
source: reading it as JSON gives the full cached `OciImage`, including
the digest used to detect image changes across runs. When the digest
changes the overlay upper layer is reset.

### Locking

`sandbox/lock` contains the running PID. If the lock file exists and the PID
is alive, `airlock up` refuses to start (one VM per project at a time). Stale
locks (dead PID) are silently cleared.


## Configuration system

### Loading order

Files are loaded in order; later files override earlier. TOML, JSON, and YAML
are supported (first matching extension wins per slot):

1. `~/.cache/airlock/config.toml`
2. `~/.airlock.toml`
3. `<project>/airlock.toml`
4. `<project>/airlock.local.toml`

### Merging semantics

- **Scalars**: later value wins.
- **Arrays**: concatenate (so `allow` lists from multiple rules add up).
- **Objects/tables**: merge recursively.
- **Null**: never overrides a set value (allows unsetting in lower-priority
  files without losing preset values).

### Presets

Presets are TOML fragments shipped with the binary. When a config declares
`presets = ["rust"]`, the preset is applied as a base layer, and the project
config is overlaid on top. Presets can include other presets. Circular
dependencies are detected and rejected.


## Build pipeline

```
mise run build
  ├─ build:kernel      Docker: compile Linux 6.18 from source (ARM64)
  ├─ build:supervisor  Docker: cross-compile Rust binary (musl, ARM64)
  ├─ build:rootfs      Docker: Alpine + airlockd supervisor → gzipped cpio
  └─ build (cargo)     Compile CLI, embed kernel + rootfs, codesign (macOS)
```

All sub-builds run inside Docker for reproducibility. Mise tasks declare
`sources` and `outputs` for incremental builds — a no-op rebuild takes ~2 ms.

The supervisor is a statically linked musl binary targeting `aarch64-unknown-
linux-musl`. It is copied into the Alpine initramfs during `build:rootfs`.

On macOS the final binary requires ad-hoc codesigning to run
(entitlements for hypervisor and virtualization access).
