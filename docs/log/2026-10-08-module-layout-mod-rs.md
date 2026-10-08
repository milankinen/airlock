# Module layout: `<module>/mod.rs`

Modules with submodules used the `<module>.rs` + `<module>/` layout. The
module root file and its submodules were in two places.

## Change

- Moved each `<module>.rs` that has a sibling `<module>/` directory to
  `<module>/mod.rs`. This applies to all crates, with no exceptions
  (31 files in `airlock-cli`, `airlockd` and `airlock-monitor`).
- Leaf modules without submodules stay as `<module>.rs`.
- No code change was necessary. Module resolution is the same, and no
  moved file uses relative `include_*!` paths or `#[path]`.
- `CLAUDE.md` now gives `src/test_cfg/mod.rs` as the helper location.
- Old plans in `docs/plans/` keep their historical paths.
