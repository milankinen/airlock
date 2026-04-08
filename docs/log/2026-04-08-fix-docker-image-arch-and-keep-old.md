# Fix: Docker image arch detection and KeepOld prompt bug

## Bug 1: Wrong-arch Docker image not detected at extraction phase

When an image reference was found in the local Docker daemon, `resolve_image()`
accepted it unconditionally regardless of architecture. On an arm64 host with an
amd64 Docker image (or vice versa), the image would be exported and used as the
VM rootfs, causing silent failures or crashes inside the VM at runtime.

**Fix:** After `image_exists()` succeeds, call `docker image inspect
--format {{.Architecture}}` to get the image arch. Compare against the host
arch (`std::env::consts::ARCH` mapped to Docker naming: x86_64→amd64,
aarch64→arm64). If they don't match, log a message and fall through to registry
resolution so the correct-arch image is pulled.

If `image_arch()` returns nothing (inspect failed), we assume the arch is
correct and proceed with Docker — this preserves backwards compatibility with
older Docker versions or unusual setups.

## Bug 2: KeepOld prompt still triggers new image extraction

When the image digest changed and the user chose "Continue using old
environment", `digest_changed` was set to `false` but `image.digest` still held
the new digest. `ensure_image()` was then called with the new digest, creating a
fresh cache directory and starting to download/extract the new image — while the
old rootfs was left untouched (since `Recreate` was not chosen). This caused
image directories to accumulate in the cache with each run.

**Fix:** In the `KeepOld` branch, verify the old image cache is still intact
(`rootfs/` + `.complete` exist), then set `image.digest` back to the old
digest. `ensure_image()` then early-returns from the existing cache directory
without downloading anything.

Edge case: if the old cache is somehow missing (manually deleted, etc.),
we fall through and use the new image — better than erroring out on something
the user can't easily fix.
