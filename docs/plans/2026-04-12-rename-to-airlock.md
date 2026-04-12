# Rename ezpez → airlock

## Context
Full project rename. Every occurrence of "ez"/"ezpez"/"ezd" in identifiers,
strings, paths, and file names must become "airlock"/"airlockd".

User-specified mappings:
- CLI binary: `ez` → `airlock`
- Daemon binary: `ezd` → `airlockd`
- Cache dir: `~/.ezpez` → `~/.cache/airlock`
- Config files: `ez.toml` → `airlock.toml`, `ez.local.toml` → `airlock.local.toml`, `~/.ez.toml` → `~/.airlock.toml`

---

## Phase 1 — Directory / file renames (git mv)

```
git mv crates/ez      crates/airlock
git mv crates/ezd     crates/airlockd
git mv mise/tasks/build/ezd  mise/tasks/build/airlockd
git mv ez.toml        airlock.toml
git mv ez.local.toml  airlock.local.toml
```

`.tmp/ez.toml` — leave (temp file, not tracked).

---

## Phase 2 — Cargo.toml files

### `/Cargo.toml` (workspace root)
- `members = ["crates/ez", "crates/common", "crates/ezd"]`
  → `["crates/airlock", "crates/common", "crates/airlockd"]`
- `ezpez-protocol = { path = "crates/common" }`
  → `airlock-protocol = { path = "crates/common" }`

### `crates/airlock/Cargo.toml`
- `name = "ezpez-cli"` → `airlock-cli`
- `[[bin]] name = "ez"` → `airlock`
- `#MISE outputs=["target/debug/ez"]` (in build/dev, not here)
- dep `ezpez-protocol` → `airlock-protocol`

### `crates/airlockd/Cargo.toml`
- `name = "ezpez-supervisor"` → `airlock-supervisor`
- `[[bin]] name = "ezd"` → `airlockd`
- dep `ezpez-protocol` → `airlock-protocol`

### `crates/common/Cargo.toml`
- `name = "ezpez-protocol"` → `airlock-protocol`

---

## Phase 3 — Rust source files

### All files: `use ezpez_protocol::` → `use airlock_protocol::`
Files (from grep):
- `crates/airlock/src/rpc/process.rs`
- `crates/airlock/src/rpc/stdin.rs`
- `crates/airlock/src/rpc/logging.rs`
- `crates/airlock/src/rpc/supervisor.rs`
- `crates/airlock/src/vm/cloud_hypervisor.rs`
- `crates/airlock/src/vm.rs`
- `crates/airlock/src/cli_server.rs`
- `crates/airlock/src/cli/cmd_exec.rs`
- `crates/airlock/src/cli/cmd_go.rs`
- `crates/airlock/src/network/tls.rs`
- `crates/airlock/src/network/tcp.rs`
- `crates/airlock/src/network/io.rs`
- `crates/airlock/src/network/server.rs`
- `crates/airlock/src/network/tests/test_middleware.rs`
- `crates/airlock/src/network/tests/test_http.rs`
- `crates/airlock/src/network/tests/helpers.rs`
- `crates/airlockd/src/rpc.rs`
- `crates/airlockd/src/logging.rs`
- `crates/airlockd/src/main.rs`
- `crates/airlockd/src/process.rs`
- `crates/airlockd/src/net/socket.rs`
- `crates/airlockd/src/net/proxy.rs`

### `crates/airlock/build.rs`
- `EZPEZ_ASSETS_CHECKSUM` → `AIRLOCK_ASSETS_CHECKSUM`

### `crates/airlock/src/assets.rs`
- `env!("EZPEZ_ASSETS_CHECKSUM")` → `env!("AIRLOCK_ASSETS_CHECKSUM")`
- comment: `~/.ezpez/kernel/` → `~/.cache/airlock/kernel/`

### `crates/airlock/src/cache.rs`
- `//! Paths into the \`~/.ezpez/\`` → `~/.cache/airlock/`
- `.join(".ezpez")` → `.join(".cache").join("airlock")`

### `crates/airlock/src/config/load_config.rs`
- `home.join(".ezpez/config.toml")` → `home.join(".cache/airlock/config.toml")`
- `home.join(".ez.toml")` → `home.join(".airlock.toml")`
- `project_root.join("ez.toml")` → `project_root.join("airlock.toml")`
- `project_root.join("ez.local.toml")` → `project_root.join("airlock.local.toml")`
- update doc comment listing the four config paths

### `crates/airlock/src/cli.rs`
- `"info,ez=trace,ezpez_supervisor=trace"` → `"info,airlock=trace,airlock_supervisor=trace"`
- `"warn,ez=debug,ezpez_supervisor=trace"` → `"warn,airlock=debug,airlock_supervisor=trace"`
- `"warn,ez=info,ezpez_supervisor=info"` → `"warn,airlock=info,airlock_supervisor=info"`

### `crates/airlock/src/rpc/logging.rs`
- comment: `ez::ezd` → `airlock::airlockd`
- `target: "ez::ezd"` → `target: "airlock::airlockd"` (3 occurrences)

### `crates/airlock/src/vm.rs`
- `ezpez.epoch=` → `airlock.epoch=`
- `ezpez.shares=` → `airlock.shares=`
- `ezpez.host_ports=` → `airlock.host_ports=`

### `crates/airlock/src/project.rs`
- `"ezpez CA"` → `"airlock CA"`

### `crates/airlock/src/network/tls.rs`
- `format!("ezpez {hostname}")` → `format!("airlock {hostname}")`

### `crates/airlock/src/oci/credentials.rs`
- `"ezpez-registry"` (×2) → `"airlock-registry"`
- comment: `~/.ezpez/` → `~/.cache/airlock/`

### `crates/airlock/src/vm/apple.rs`
- `"com.ezpez.vm"` → `"com.airlock.vm"`

### `crates/airlockd/src/init/linux.rs`
- `"ezpez-disk"` → `"airlock-disk"`

---

## Phase 4 — VM init scripts

### `vm/initramfs/build.sh`
- `/usr/bin/ezd` → `/usr/bin/airlockd` (×2, cp + chmod)

### `vm/initramfs/init`
- `hostname ezvm` → `hostname airlock`
- `/usr/bin/ezd` → `/usr/bin/airlockd`

---

## Phase 5 — Build system

### `mise.toml`
- `[tasks.ez]` → `[tasks.airlock]`
- `exec target/debug/ez` → `exec target/debug/airlock`
- `exec target/debug/ez go --login` (in claude task) → `target/debug/airlock go --login`

### `mise/tasks/build/dev`
- `#MISE outputs=["target/debug/ez"]` → `target/debug/airlock`
- `cargo build -p ezpez-cli` → `airlock-cli`
- `codesign ... target/debug/ez` → `target/debug/airlock`

### `mise/tasks/build/airlockd` (was `ezd`)
- MISE description: update
- MISE sources: `crates/ezd/` → `crates/airlockd/`
- MISE outputs: `target/vm/ezd` → `target/vm/airlockd`
- All `ezd` → `airlockd` (binary name in paths)
- `ezpez-supervisor` → `airlock-supervisor`
- `ezpez-supervisor-builder` → `airlock-supervisor-builder`
- Docker volumes: `ezpez-cargo-cache` → `airlock-cargo-cache`, `ezpez-target-cache` → `airlock-target-cache`

### `mise/tasks/build/dev-image`
- `target/ez.dev` → `target/airlock.dev`
- `-t ez:dev` → `-t airlock:dev`

### `.worktreeinclude`
- `ez.local.toml` → `airlock.local.toml`

---

## Phase 6 — install.sh

- `REPO="milankinen/ezpez"` → `milankinen/airlock`
- `EZPEZ_INSTALL_DIR` → `AIRLOCK_INSTALL_DIR` (×2)
- `EZPEZ_VERSION` → `AIRLOCK_VERSION` (×2)
- Archive: `ez-${VERSION}` → `airlock-${VERSION}`
- `mv "$TMPDIR/ez"` → `mv "$TMPDIR/airlock"` (×2)
- Print messages: `ez` → `airlock`

---

## Phase 7 — GitHub workflows

### `.github/workflows/ci.yml`
- `Build ezd` step names → `Build airlockd`
- `mise run build:ezd` → `mise run build:airlockd`
- `cargo build --release -p ezpez-cli` → `airlock-cli` (×6)
- `target/release/ez` → `target/release/airlock` (×6 codesign + artifact paths)
- Artifact names: `ez-macos-aarch64`, `ez-linux-*` → `airlock-macos-aarch64`, `airlock-linux-*` (×12)

### `.github/workflows/release.yml`
- Artifact download loop: `ez-macos-aarch64`, etc. → `airlock-*`
- `${name#ez-}` dir strip → `${name#airlock-}`
- Archive: `ez-${VERSION}` → `airlock-${VERSION}` (all occurrences)
- `chmod +x .../ez` → `airlock`
- `tar ... ez` → `airlock`
- Release asset list: all `ez-${VERSION}-*.tar.gz` → `airlock-*`
- `sed` command updating `install.sh` DEFAULT_VERSION stays same

---

## Phase 8 — CLAUDE.md

- `ez go` → `airlock go`
- `ez exec`, `ez x`, `ez project` → `airlock exec`, `airlock x`, `airlock project`
- `target/debug/ez` → `target/debug/airlock`
- `mise run ez` → `mise run airlock`

---

## Verification

```bash
# Build should succeed
mise run build:dev

# No remaining ez/ezpez references in code (excluding docs/log which can be left as history)
grep -r "ezpez\|\.join(\".ezpez\"\|EZPEZ_\|\"ez::\|\"com\.ezpez\|\"ezpez-" crates/ mise/ vm/ *.toml *.sh

# Check binary name
target/debug/airlock --version
```
