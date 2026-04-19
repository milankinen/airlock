# Flat `app/` layout

The repo had grown two parallel roots for first-party code: `crates/`
for Rust packages (airlock CLI, supervisor, shared protocol, TUI) and
`vm/` for the two non-Rust build units that produce the guest kernel
and initramfs. The split was arbitrary — both directories contained
things that ship together, and navigating between them required
remembering which name lived where. It also made workspace-wide
tooling (mise tasks, CI cache keys, docs cross-references) carry two
prefixes that mean the same thing.

Everything now lives under a single flat `app/` directory with
consistent names:

- `crates/airlock`  → `app/airlock-cli`    (package `airlock-cli`)
- `crates/airlockd` → `app/airlockd`       (package `airlock-supervisor` → `airlockd`)
- `crates/common`   → `app/airlock-common` (package `airlock-protocol`   → `airlock-common`)
- `crates/tui`      → `app/airlock-monitor`(package `airlock-tui`        → `airlock-monitor`)
- `vm/kernel`       → `app/vm-kernel`
- `vm/initramfs`    → `app/vm-initramfs`

Package renames went along with the path moves where the old name no
longer matched the directory. `airlock-supervisor` was the guest-side
daemon binary — the binary has always been named `airlockd`, so the
package name now matches. `airlock-protocol` was a misleading name
anyway: the crate holds the capnp schemas *and* a pile of shared
constants/types that aren't protocol-level. `airlock-common` is a
more honest description. `airlock-tui` becomes `airlock-monitor` to
match the F2 tab rename that landed earlier.

## Mechanical impact

- Workspace `Cargo.toml` members + internal path deps updated.
- `tracing` log filter strings in `cli.rs` went from
  `airlock_supervisor` to `airlockd` (EnvFilter targets follow the
  package name with hyphens→underscores).
- All mise tasks (`build:kernel`, `build:initramfs`, `build:airlockd`,
  `build:dev`, `build:release`) updated to the new source paths.
- CI `hashFiles()` kernel cache keys point at `app/vm-kernel/` now.
- Relative `include_bytes!` and `std::fs::read` paths in
  `airlock-cli` (`build.rs`, `assets.rs`) didn't need to change — the
  depth from crate root to `target/vm/` is identical after the move.
- `docs/manual/src/advanced/custom-kernel.md` now points users at
  `app/vm-kernel/` for the build script and kernel configs.

Historical `docs/log/` and `docs/plans/` entries were left alone —
they describe state at the time they were written.
