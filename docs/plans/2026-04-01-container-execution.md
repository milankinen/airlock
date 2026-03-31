# Run Alpine container via crun + VirtioFS

## Context

Currently `exec` spawns `/bin/sh` directly in the VM. We want to run
an OCI container inside the VM for proper isolation. The host CLI
prepares an Alpine OCI bundle on disk, shares it into the VM via
VirtioFS, and the supervisor launches `crun` instead of a bare shell.

## Architecture

```
Host (macOS)                              VM (Linux)
┌──────────────────┐                      ┌────────────────────┐
│ CLI              │                      │ init               │
│ ├─ prepare_bundle│                      │ ├─ mount virtiofs  │
│ │  ~/.ezpez/     │──VirtioFS "bundle"──→│ │  /mnt/bundle     │
│ │  bundles/      │                      │ ├─ mount cgroup2   │
│ │  alpine/       │                      │ └─ supervisor      │
│ │   config.json  │                      │    └─ crun run     │
│ │   rootfs/      │                      │       --bundle     │
│ └─ boot VM       │                      │       /mnt/bundle  │
└──────────────────┘                      └────────────────────┘
```

## Plan

### Phase 1: Kernel config — enable namespaces + cgroups

The current kernel has all namespaces disabled and no cgroup support.
crun needs at minimum PID namespace and cgroups.

Add to `sandbox/kernel/config-arm64`:
```
CONFIG_CGROUPS=y
CONFIG_CGROUP_DEVICE=y
CONFIG_CGROUP_PIDS=y
CONFIG_CGROUP_FREEZER=y
CONFIG_MEMCG=y
CONFIG_CPUSETS=y
CONFIG_CGROUP_CPUACCT=y
CONFIG_CGROUP_SCHED=y
CONFIG_PID_NS=y
CONFIG_UTS_NS=y
CONFIG_NET_NS=y
CONFIG_USER_NS=y
CONFIG_IPC_NS=y
CONFIG_OVERLAY_FS=y
```

Seccomp stays disabled (crun works without it if config.json omits
the seccomp section).

**Files:** `sandbox/kernel/config-arm64`
**Verify:** Boot, `cat /proc/cgroups` shows subsystems, `ls /proc/self/ns/` shows namespaces.

### Phase 2: Add crun to rootfs + mount setup

Add `crun` package and the mounts it needs.

**`sandbox/rootfs/build.sh`:** add `crun` to `apk add`
**`sandbox/rootfs/init`:** add before supervisor:
```sh
mount -t tmpfs none /tmp
mount -t tmpfs none /run
mkdir -p /sys/fs/cgroup
mount -t cgroup2 none /sys/fs/cgroup
mkdir -p /mnt/bundle
mount -t virtiofs bundle /mnt/bundle || true
```

The `|| true` on virtiofs mount handles the case when no VirtioFS
share is configured (e.g., during development/testing).

**Verify:** Boot, `which crun`, `crun --version` works.

### Phase 3: OCI bundle preparation via mise task

New mise task `build:bundle` prepares `.tmp/bundle/` with rootfs +
config.json. No Rust code needed.

**New file `sandbox/bundle/build.sh`:**
- Downloads `alpine-minirootfs-3.23.3-aarch64.tar.gz`
- Extracts into `.tmp/bundle/rootfs/`
- Writes `config.json` (minimal OCI runtime spec)

**New mise task `build:bundle`:**
- Sources: `sandbox/bundle/build.sh`, `sandbox/bundle/config.json`
- Outputs: `.tmp/bundle/rootfs/`, `.tmp/bundle/config.json`
- `build` task depends on it

**config.json** — static file in `sandbox/bundle/config.json`:
- `process.args: ["/bin/sh", "-l"]`
- `process.terminal: false` (supervisor manages PTY)
- `root.path: "rootfs"`
- Basic mounts: proc, dev, devpts, shm, sysfs
- Namespaces: pid, ipc, uts, mount
- No seccomp

CLI hard-codes `.tmp/bundle` as the VirtioFS share path.

**Files:** new `sandbox/bundle/build.sh`, `sandbox/bundle/config.json`,
`mise.toml`
**Verify:** `mise run build:bundle` creates `.tmp/bundle/` with files.

### Phase 4: VirtioFS device in VM config

Add `VZVirtioFileSystemDeviceConfiguration` to share the bundle dir.

**`cli/Cargo.toml`:** add features:
```
VZVirtioFileSystemDeviceConfiguration
VZDirectorySharingDeviceConfiguration
VZDirectoryShare
VZSharedDirectory
VZSingleDirectoryShare
```

**`cli/src/vm/config.rs`:** add `bundle_path: Option<PathBuf>` to VmConfig

**`cli/src/vm/apple.rs`:** in `create_vm_config()`, if bundle_path
is Some, add VirtioFS device with tag `"bundle"` pointing to it.

**`cli/src/main.rs`:** hard-code `bundle_path: Some(".tmp/bundle".into())`.

**Verify:** Boot with verbose, `ls /mnt/bundle/rootfs/` shows Alpine
files inside the VM.

### Phase 5: Supervisor launches crun instead of /bin/sh

In `sandbox/supervisor/src/main.rs` exec handler, change:
```rust
// Before:
pty_process::Command::new("/bin/sh").arg0("-sh")
// After:
pty_process::Command::new("crun")
    .args(["run", "--bundle", "/mnt/bundle", "ezpez0"])
```

Container ID `"ezpez0"` is fixed (one container per VM).
crun state goes in `/run/crun/ezpez0/`.

Signal forwarding works transparently — signals to crun are
forwarded to the container's init process.

**Verify:** `mise run ez` → shell prompt inside Alpine container.
`cat /etc/os-release` shows Alpine. `ps aux` shows containerized PIDs.
`exit` cleanly shuts down.

## Risks

- **crun + PTY interaction:** `terminal: false` in config.json means
  crun inherits the supervisor's PTY. If this doesn't work, fall back
  to `terminal: true` + `--console-socket`.
- **VirtioFS permissions:** files created by macOS user may have
  wrong uid inside VM. Should work since VM runs as root.
- **Kernel config changes:** `make olddefconfig` resolves new
  dependencies. Verify boot before proceeding.
