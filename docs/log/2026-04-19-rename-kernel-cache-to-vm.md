# Rename the VM assets cache dir from `kernel/` to `vm/`

`~/.cache/airlock/kernel/` was the extraction target for every asset
embedded in the `airlock` binary: the Linux `Image`, the initramfs
archive, and (on Linux) the `cloud-hypervisor` and `virtiofsd`
executables. Calling that directory `kernel/` was a misnomer — only
one of the four files is actually a kernel.

Renamed to `vm/`, which matches how `target/vm/` is already laid out
on the build side and reads as "everything needed to boot the VM".
One in-code path change in `assets.rs`; module doc comments in
`assets.rs` / `cache.rs` and the `DESIGN.md` boot-assets section
updated to match.

## Migration

Asset extraction keyed on `AIRLOCK_ASSETS_CHECKSUM` stored next to
the files. Since the new `vm/` directory starts without a checksum,
the first run after upgrade re-extracts everything into `vm/`. The
old `kernel/` dir sits orphaned and can be deleted by hand. This is
the same pattern we used for the earlier `~/.cache/airlock/` →
`~/.cache/airlock/oci/` move — cache is rebuildable and a
compatibility shim would live forever.
