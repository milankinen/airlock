# Gate kernel/assets per platform, add Linux CI and release

Cleaned up the build pipeline so each platform only embeds what it needs.

### Platform-specific assets

- **macOS**: embeds kernel Image + initramfs.gz (for Apple Virtualization)
- **Linux**: embeds rootfs.tar.gz + libkrun.so + libkrunfw.so (no kernel — 
  libkrunfw provides it)
- Rootfs build now produces two formats: gzipped cpio (macOS) and gzipped
  tar (Linux). The tar is extracted at runtime using the `tar` + `flate2`
  crates already in deps.
- `build.rs` and `assets.rs` fully gated with `#[cfg(target_os)]` — no
  placeholder files needed.
- The `initramfs` field in Assets is reused: points to initramfs.gz on
  macOS, extracted rootfs directory on Linux.

### CI pipeline

Added x86_64 and aarch64 Linux build jobs:
- `build-rootfs-x86_64` / `build-libkrun-x86_64` (stage 2)
- `build-rootfs-aarch64` already existed, now uploads both formats
- `build-libkrun-aarch64` (stage 2)
- `build-cli-linux-x86_64` / `build-cli-linux-aarch64` (stage 3)
- Docker builds now `chown` outputs to host UID/GID

### Release and install

- Release workflow packages three variants: darwin-aarch64, linux-x86_64,
  linux-aarch64
- `install.sh` supports Linux + macOS, validates platform combinations,
  uses `sha256sum` on Linux / `shasum` on macOS
