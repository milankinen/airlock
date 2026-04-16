# Bats end-to-end test harness

### What

Add a bats-core test harness with CLI and VM integration tests,
restructured mise tasks, and a CI job that runs VM tests on both
x86_64 and aarch64 Linux runners with KVM.

### Structure

```
tests/
  helpers.bash          # Shared setup/teardown/assertions/run_airlock
  cli/
    helpers.bash        # Proxy to ../helpers.bash
    help.bats           # CLI help, version, invalid args
    config.bats         # Config parsing, presets, merging, errors
    commands.bats       # start/exec/show/rm error paths
  vm/
    helpers.bash        # VM-specific: require_vm_support, run_vm
    boot.bats           # VM boot, exit codes, working directory
    env.bats            # Env var injection, local override, substitution
    mounts.bats         # Directory/file mounts, rw/ro, sync
    network.bats        # Default deny, port forwarding, HTTP/HTTPS
    middleware.bats     # Lua scripting: path-based deny
```

### Key design decisions

- `run_vm` wraps `airlock --quiet start -- <cmd>` — each test boots
  a fresh VM but reuses the cached sandbox state via `FILE_TEMP_DIR`
- Temp dirs under `.tmp/tests/` with `AIRLOCK_TEST_KEEP=1` support
- `AIRLOCK_BIN` env var overrides binary path (used in CI)
- `build:release` file task builds with `--release` and codesigns on
  macOS
- Renamed `fetch:virtiofsd` → `build:virtiofsd` (we actually build it)
- CI `bats-vm` job uses matrix strategy for x86_64 + aarch64, enables
  KVM via udev rule, downloads pre-built artifacts
