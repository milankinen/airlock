# Update CI pipelines for cloud-hypervisor migration

The CI workflows still referenced the old libkrun VMM which was replaced
by cloud-hypervisor + virtiofsd. Several jobs were broken or missing:

- `build-libkrun-*` jobs ran `mise run build:libkrun` which no longer
  exists. Replaced with `fetch-vmm-*` jobs that run
  `fetch:cloud-hypervisor` and `fetch:virtiofsd`.
- No kernel build for x86_64 existed even though `config-x86_64` was
  present and the CLI embeds the kernel. Added `build-kernel-x86_64`.
- `build-rootfs-x86_64` only uploaded `rootfs.tar.gz` but missed
  `initramfs.gz` which is embedded via `include_bytes!`.
- Linux CLI build jobs depended on and downloaded the nonexistent
  libkrun artifacts. Updated to depend on kernel + rootfs + vmm
  artifacts instead.
