# Run Alpine container via crun + VirtioFS

### What

The shell now runs inside an OCI container (Alpine) managed by `crun`,
with the container bundle prepared on the host and shared into the VM
via VirtioFS.

### Architecture

```
Host: .tmp/bundle/ ──VirtioFS──→ VM: /mnt/bundle
  config.json                      crun run --no-pivot --bundle /mnt/bundle
  rootfs/ (Alpine minirootfs)      → containerized /bin/sh with PID namespace
```

### Changes

- **Kernel config**: enabled namespaces (PID, UTS, NET, USER, IPC,
  mount) and cgroups (pids, memory, cpu, freezer, devices). Required
  by crun for container isolation.
- **Rootfs**: added `crun` package, cgroup2 mount, VirtioFS mount
  at `/mnt/bundle`.
- **VirtioFS**: `VZVirtioFileSystemDeviceConfiguration` shares the
  host bundle directory into the VM with tag `"bundle"`.
- **Bundle**: mise task `build:bundle` downloads Alpine minirootfs,
  creates OCI runtime bundle at `.tmp/bundle/` with `config.json`.
- **Supervisor**: `exec` now runs `crun run --no-pivot --bundle
  /mnt/bundle ezpez0` instead of `/bin/sh`.

### Key decisions

- **`--no-pivot`** — VirtioFS (FUSE-based) doesn't support the
  `pivot_root` syscall. `--no-pivot` uses `chroot` instead, which is
  fine since the VM is the real security boundary, not the container.
- **`terminal: true`** in config.json — crun creates a PTY for the
  container process, fixing the "can't access tty" warning.
- **Hard-coded Alpine** — MVP uses a fixed Alpine minirootfs. Dynamic
  image pulling (via `oci-client` crate) is a future step.
- **No IPC namespace** — kernel's `CONFIG_IPC_NS` requires
  `CONFIG_SYSVIPC` which wasn't enabled. Omitted from config.json;
  not essential for MVP.
- **Hand-written config.json** — static 40-line OCI spec. No
  `oci-spec` crate needed for a fixed config.
