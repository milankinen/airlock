# Split init/linux.rs by stage

`app/airlockd/src/init/linux.rs` hit ~765 lines as the guest-init
responsibility kept growing: clock, VirtioFS mounts, networking,
project-disk format/mount, overlayfs assembly, CA bundle merging,
container mounts, and a pile of shared mount-syscall wrappers all
lived in one file. The review flagged it as a mechanical split — each
stage is self-contained, the mount helpers are pure wrappers, and the
CA injection code carries enough distro knowledge to warrant its own
file.

## Layout

```
src/init/linux.rs             # sequencer: mod decls + setup()
src/init/linux/clock.rs       # set guest system clock
src/init/linux/mount.rs       # mount(2) wrappers: virtiofs, bind, bind_rec, fs
src/init/linux/net.rs         # iptables redirect + /etc/resolv.conf
src/init/linux/disk.rs        # /dev/vda mkfs.ext4 + /mnt/disk setup
src/init/linux/overlay.rs     # overlayfs assembly + file symlinks + dir/cache binds
src/init/linux/ca.rs          # project CA → tmpfs lowerdir + anchor paths
src/init/linux/container.rs   # proc/sys/dev, cgroup2, /airlock/disk, file binds
```

## Visibility

Everything moved out of `linux.rs` is `pub(super)` — visible to
`linux.rs` (the sequencer) and reachable from sibling stages through
`super::mount::...` without widening any API beyond the `init::linux`
subtree.

Siblings that need shared syscall wrappers (`mount::fs`,
`mount::bind`, `mount::bind_rec`) call them as `super::mount::fs`.
The CA stage (`ca::prepare_overlay`) is called by `overlay::assemble`
with a single `super::ca::prepare_overlay(mounts)?`, keeping the CA
internals out of the overlay file.

## Renames

A few functions got shorter names now that the module path disambiguates:

- `set_clock` → `clock::set`
- `mount_virtiofs` / `mount_virtiofs_at` → `mount::virtiofs` / `mount::virtiofs_at`
- `bind_mount` / `bind_mount_rec` / `mount_fs` → `mount::bind` / `mount::bind_rec` / `mount::fs`
- `setup_networking` → `net::setup`; `setup_dns` → `net::setup_dns`
- `setup_disk` → `disk::setup`
- `assemble_rootfs` / `reset_overlay_if_needed` → `overlay::assemble` / `overlay::reset_if_image_changed`
- `prepare_ca_overlay` / `find_bundle_in_layers` / `write_ca_bundle` →
  `ca::prepare_overlay` / `ca::find_bundle_in_layers` / `ca::write_bundle`
- `setup_container_mounts` → `container::setup`

## No behavior change

Pure refactor. `setup()` in `linux.rs` calls the same steps in the
same order — the only difference is the functions live in sibling
files. `cargo test -p airlockd` passes unchanged.
