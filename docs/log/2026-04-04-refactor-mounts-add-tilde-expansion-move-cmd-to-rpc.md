# Refactor mounts, add tilde expansion, move cmd to RPC

### Mount resolution refactoring

Separated mount resolution from execution:

- `oci::prepare` resolves everything: tilde expansion on source
  (host `~`) and target (container `~` via rootfs `/etc/passwd`),
  mount type detection (`Dir`/`File`), OCI config.json generation.
  Returns `Bundle` with `Vec<ResolvedMount>`.
- `vm::start` reads `bundle.mounts` to add VirtioFS shares, hardlink
  files, and build the kernel cmdline. No more `PreparedMounts` /
  `mounts.rs` — removed in favor of direct mount handling in `vm.rs`.
- `ResolvedMount` carries display paths (original with `~`) separate
  from resolved paths, plus `MountType::Dir { key }` / `File { filename }`
  with `key()` and `vm_path()` helpers.

Fixed bug: OCI config.json was written before config mounts were
added, so crun never saw user mounts. Now all mounts are assembled
before `generate_config`.

### Supervisor command via RPC

Added `cmd :Text` and `args :List(Text)` to the supervisor start
RPC. CLI builds the crun command and sends it — supervisor just
executes whatever it receives. Added `dev` cargo feature: with
`EZ_DEV_NO_CRUN=true` env var, sends `/bin/sh` instead of crun
for debugging the VM without the container.
