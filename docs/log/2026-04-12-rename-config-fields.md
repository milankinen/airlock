# Rename config fields for clarity

Two config renames for cleaner project host folder layout and shorter config keys:

1. **`files_rw` / `files_ro` → `files/rw` / `files/ro`**: The overlay
   subdirectories that stage file mounts on the host now use a nested
   `files/` parent instead of flat underscore-separated names. This
   matches the guest-side layout (`/ez/.files/rw`, `/ez/.files/ro`)
   and keeps the overlay directory tidier as more subdirs are added.

2. **`nested_virtualization` → `kvm`**: The VM config field is shorter
   and more direct — it controls whether `nested=on` is passed to
   cloud-hypervisor's `--cpus` flag, which is KVM-specific.
