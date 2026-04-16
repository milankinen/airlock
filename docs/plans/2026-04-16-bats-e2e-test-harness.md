# Plan: Bats E2E Test Harness for Airlock CLI

## Context

The airlock CLI has unit tests for config merging, network, and mounts, but no end-to-end tests exercising the actual binary. We just fixed two bugs (config loading, exec env vars) that would have been caught by e2e tests. This harness tests CLI arg parsing, config loading/merging/validation, and error paths — all without booting a VM.

## Structure

```
tests/
  helpers.bash          # Shared setup/teardown/assertions
  cli_help.bats         # Help, version, invalid args
  config_loading.bats   # Config parsing, presets, merging, errors
  start.bats            # Start command error paths
  exec.bats             # Exec without running VM
  show.bats             # Show without project data
  rm.bats               # Rm behavior
```

Each test case runs in an isolated temp directory with `HOME` overridden so no real user config interferes.

## Changes

### 1. Non-interactive mode for `airlock start`

Remove the TTY gate at the top of `cmd_start.rs::run()`. Instead, check for interactive mode only where it's actually needed (dialoguer prompts). This lets non-interactive runs proceed through config loading, validation, etc.

### 2. `mise.toml` — Add bats + restructure test tasks

### 3. `tests/helpers.bash` — Shared test infrastructure

### 4. Test files covering CLI help, config loading, start, exec, show, rm
