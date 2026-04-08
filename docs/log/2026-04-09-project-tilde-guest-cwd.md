# Project tilde helpers and --project-cwd support

**Date:** 2026-04-09

## Changes

### `project.host_home` / `expand_host_tilde`

`dirs::home_dir()` was previously called ad-hoc in `oci.rs` and `network.rs`
with `unwrap_or_default()`. Home directory resolution is now done once in
`project::load` and `project::lock`, stored as `Project.host_home`, and
propagates an error if the home directory cannot be determined (rather than
silently using an empty path).

`Project::expand_host_tilde(&self, path)` is an instance method using
`self.host_home`. `Bundle::expand_tilde(&self, path)` does the same for
container paths using the container's home directory.

`network::setup` now uses these helpers instead of its own local
`expand_tilde` function plus `dirs::home_dir()`.

### `project.host_cwd` / `project.guest_cwd`

`Project.cwd` split into:
- `host_cwd` — absolute path on the host (used for mount source, config
  loading, relative path resolution)
- `guest_cwd` — working directory inside the container (defaults to
  `host_cwd`, overridable via `--project-cwd`)

`guest_cwd` is persisted to `<cache>/guest_cwd` on each `ez go` start, so
`ez exec` (which loads the project without starting it) can default to
the same directory.

`display_cwd()` returns `host_cwd` when both are equal, or
`host_cwd → guest_cwd` when they differ.

### `ez go --project-cwd <path>`

New CLI option to override the container working directory. Useful when
the host path structure doesn't match what the container expects (e.g.,
mounting `/Users/me/myproject` but wanting cwd `/app` inside).
