# Fix airlock exec not applying config environment variables

## Problem

`airlock exec` only forwarded environment variables passed via CLI
`-e KEY=VALUE` flags. Environment variables defined in the `[env]`
config section (from `airlock.toml` / `airlock.local.toml`) were
completely ignored for exec'd processes.

## Root cause

`cmd_exec.rs` loaded the project config but never read
`project.config.env`. Only the CLI-provided `env` vec was serialized
into the RPC request.

## Fix

Added `resolve_config_env()` which merges config env vars (with
`${VAR}` host substitution) with CLI-passed env vars before sending
the RPC request. CLI `-e` flags take precedence over config values
for the same key. This mirrors the `resolve_env()` logic already
used by the `start` path in `vm.rs`.
