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
│  │  ├─ Mount VirtioFS shares (base image, overlay, mounts)  │   │
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

CliService   (Unix socket: <project-cache>/cli.sock)
  exec(stdin, pty, cmd, args, cwd, env) → Process
    # Exposed by `airlock go` for `airlock exec` to connect to.

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

1. **Mount VirtioFS shares** — `base` (read-only image rootfs) and `overlay`
   (CA certs, file mount staging dirs) are mounted unconditionally. The full
   mount list is received via the `start` RPC (no separate `mounts.json`).

2. **Set system clock** — host passes a Unix epoch in the start RPC so the
   guest clock is correct from the start.

3. **Setup networking** — loopback IP `10.0.0.1/8`, default route, iptables
   rules for localhost port forwarding (port N → 15001, the in-VM TCP proxy).

4. **Mount the project disk** — formats the ext4 image if blank (`mkfs.ext4`),
   then mounts it at `/mnt/disk`. Resizes the filesystem if the disk image was
   enlarged.

5. **Assemble overlayfs rootfs**:
   - **lower**: CA cert layer (`/mnt/overlay/ca`) if present, then base image
     (`/mnt/base`). Leftmost = highest priority.
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

1. `airlock exec` connects to `<project-cache>/cli.sock` (Unix socket, Cap'n
   Proto RPC) that `airlock go` exposes while the VM is running.
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

1. The file is hard-linked into a staging directory (`overlay/files/rw/` or
   `overlay/files/ro/`) which is the VirtioFS share root.
2. Inside the rootfs, a symlink at the target path points into
   `/airlock/.files/rw` or `/airlock/.files/ro`.

Hard links provide bidirectional sync — changes in the container appear on
the host and vice versa. If hard-linking fails (cross-filesystem), the file
is copied with a warning that sync is one-way.

### CA cert layer

The project CA certificate (used for TLS interception) is installed as an
extra overlayfs lowerdir rather than a symlink or file mount. The cert bundle
is written to `overlay/ca/` mirroring the distro's CA store paths (e.g.
`overlay/ca/etc/ssl/certs/ca-certificates.crt`). The supervisor prepends
`/mnt/overlay/ca` as the highest-priority lowerdir:

```
lowerdir=/mnt/overlay/ca:/mnt/base
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

Decision logic:
1. If any `deny` pattern matches → **block** immediately (deny wins).
2. If any `allow` pattern matches → **allow**; collect middleware from all
   matching allow rules.
3. If neither matched → follow `default_mode` (`"allow"` by default, or
   `"deny"` to require explicit allow rules).

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

TLS interception is applied **only** for hosts that have Lua middleware
scripts attached. Hosts without middleware (or unmatched hosts under
`default_mode = "allow"`) pass through as raw TCP — no TLS MITM.

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

### Identity and cache

Each project is identified by the SHA256 hash of its canonical working
directory path. State is stored in `~/.cache/airlock/projects/<hash>/`:

```
<hash>/
  lock              # PID lock (one VM per project at a time)
  cwd               # Canonical working directory (for display)
  guest_cwd         # Working directory inside container (if overridden)
  image             # Last used image name
  last_run          # Unix epoch of last run
  disk.img          # Sparse ext4 image (overlay upper + caches)
  ca/
    ca.crt          # Self-signed CA cert (PEM)
    ca.key          # CA private key (PEM)
  overlay/
    files/rw/       # Hard-linked rw file mounts (VirtioFS share root)
    files/ro/       # Hard-linked ro file mounts (VirtioFS share root)
    ca/             # CA cert tree (overlayfs lowerdir)
  cli.sock          # Unix socket for airlock exec RPC
```

### Image cache

Images are cached at `~/.cache/airlock/images/<manifest-digest>/`:

```
<digest>/
  rootfs/            # All layers merged (read-only)
  image_config.json  # OCI image config (CMD, ENV, user, etc.)
  layer_*.tar.gz     # Raw layer blobs
```

The image cache is shared across all projects. Platform is fixed to
`linux/arm64` (matching the VM architecture). A stored per-project digest
file detects image changes across runs; if the digest changes the overlay
upper layer is reset.

### Locking

The lock file contains the running PID. If a lock file exists and the PID is
alive, `airlock go` refuses to start (one VM per working directory). Stale
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
