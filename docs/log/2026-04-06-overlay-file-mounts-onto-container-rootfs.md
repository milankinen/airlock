# Overlay file mounts onto container rootfs

Individual file bind mounts via crun had permission issues — files were
visible (`ls` showed correct ownership/mode) but reads failed with
"Permission denied" inside the container despite running as root with all
capabilities. Directory mounts via VirtioFS worked fine.

### Root cause

crun's bind mount of individual files from a VirtioFS share into the
container didn't work reliably. Reading the file from the VM root
(`/mnt/files_rw/.claude.json`) succeeded, but the same file bind-mounted
into the container (`/root/.claude.json`) was unreadable.

### Fix: overlayfs + per-file bind mounts

Instead of per-file bind mounts in the OCI config, file mounts now use a
two-layer approach in the VM init script:

1. `link_file` replicates the target directory structure inside the
   `files_rw`/`files_ro` wrapper: target `/root/.claude.json` becomes
   `files_rw/root/.claude.json`.
2. Init sets up overlayfs on the container rootfs:
   `lowerdir=files_rw:files_ro:rootfs, upperdir=tmpfs, workdir=tmpfs`.
   This makes all mounted files visible at their target paths. The tmpfs
   upperdir absorbs stray rootfs writes so they don't leak to the host.
3. Each file in `files_rw` is then individually bind-mounted from VirtioFS
   on top of the overlay, so writes sync back to the host.

File mounts are now excluded from the OCI config (only directory and cache
mounts remain as bind mounts).
