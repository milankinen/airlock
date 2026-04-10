# File Mount Canonicalization

## Context

Two bugs prevent file mounts with project-relative paths from working correctly:

1. **Relative target paths go to container root** — `target = "mise.toml"` resolves to `/mise.toml` in
   the container instead of `<guest_cwd>/mise.toml`. `resolve_mounts` only calls `expand_tilde` on the
   target; it doesn't join relative paths with `guest_cwd` the way it already does for sources with `cwd`.

2. **File mount symlinks are shadowed by dir bind mounts** — `assemble_rootfs` creates symlinks in the
   overlayfs upper layer *before* applying dir bind mounts. When the project dir bind mount is applied at
   `<guest_cwd>`, it covers the entire directory tree there, hiding any symlinks for file mounts whose
   targets fall under that path. The fix is to apply file mounts as direct bind mounts in
   `setup_container_mounts`, which runs *after* `assemble_rootfs` and therefore after dir bind mounts.

## Plan

### Phase 1 — Fix target path resolution (`crates/ez/src/oci.rs`)

**`resolve_mounts` (line 599):**
- Add a `guest_cwd: &Path` parameter after `cwd: &Path`
- After `let target = expand_tilde(&m.target, &container_home);`, add:
  ```rust
  let target = if target.is_relative() {
      guest_cwd.join(&target)
  } else {
      target
  };
  ```

**`build_bundle` (line 203):**
- Pass `&project.guest_cwd` as the new fifth argument to `resolve_mounts`

### Phase 2 — Switch file mounts from symlinks to bind mounts (`crates/ezd/src/init/linux.rs`)

**`assemble_rootfs` (line 184):**
- Remove the entire file-symlink block (lines 233–253): `create_dir_all` for `ez/.files/{rw,ro}`
  and the `for file in &mounts.files` symlink loop
- Keep everything else (overlayfs setup, dir bind mounts, cache bind mounts) unchanged

**`setup_container_mounts` (line 54):**
- Remove the `/ez/.files/{rw,ro}` bind mount block (lines 111–128, the `has_rw`/`has_ro` section)
- Add a new file bind mount block after the socket forward section. For each file mount:
  ```rust
  for file in &mounts.files {
      let subdir = if file.read_only { "files_ro" } else { "files_rw" };
      let rel = file.target.strip_prefix('/').unwrap_or(&file.target);
      let src = format!("/mnt/overlay/{subdir}/{rel}");
      let dst = format!("{root}/{rel}");
      if let Some(parent) = std::path::Path::new(&dst).parent() {
          std::fs::create_dir_all(parent)?;
      }
      if !std::path::Path::new(&dst).exists() {
          let _ = std::fs::File::create(&dst);
      }
      bind_mount(&src, &dst, file.read_only)?;
      debug!("file bind: {src} → {dst}");
  }
  ```

### Phase 3 — Update tests (`crates/ez/src/oci/tests/test_resolve_mounts.rs`)

- Add `guest_cwd: &Path` argument to every `resolve_mounts` call; use `Path::new("/workdir")`
- Add new test `relative_target_resolved_against_guest_cwd`

## Files changed

| File | Change |
|------|--------|
| `crates/ez/src/oci.rs` | `resolve_mounts` new `guest_cwd` param + target resolution; `build_bundle` passes `&project.guest_cwd` |
| `crates/ezd/src/init/linux.rs` | Remove symlink block from `assemble_rootfs`; replace `/ez/.files` bind mounts with per-file bind mounts in `setup_container_mounts` |
| `crates/ez/src/oci/tests/test_resolve_mounts.rs` | Update all call sites; add `relative_target_resolved_against_guest_cwd` test |

## Mount order (no changes needed)

User dir mounts are correct already: project dir is `mounts[0]`, user mounts appended via
`mounts.extend(...)`. Dir bind mounts are applied in list order, so later user dir mounts
targeting subdirectories of `guest_cwd` correctly shadow the project dir mount for those paths.
