# Simplify `airlock exec` — no project load, server-side env merge

`airlock exec` was doing far too much. It loaded the project
(`config`, vault, CA, run metadata), re-resolved `airlock.toml` env
vars through the vault, and then sent a fully-baked env list over the
cli.sock. All of that duplicated state the running sandbox already
holds: the `airlock start` process has the resolved env (image env +
`airlock.toml` env) sitting on its `VmInstance`, and the CLI socket
server runs in that same process.

The simplification moves the responsibility where the state already
lives: exec becomes a dumb client, and the CLI server merges
overrides onto the sandbox's base env before handing off to the
supervisor.

## New exec flow

1. `airlock exec` reads `current_dir()` and walks up looking for
   `.airlock/sandbox/cli.sock`. First hit wins.
2. It sends `(cmd, args, cwd, overrides)` to that socket. `cwd` is
   the caller's current directory (or `-w` override). `overrides` is
   *only* the repeated `-e KEY=VAL` flags — no config, no vault.
3. `cli_server.rs` holds a `Rc<Vec<String>>` of the sandbox's
   resolved base env (copied from `vm.env` at start time). On each
   exec it merges: for each `KEY=` in overrides, drops any prior
   `KEY=` from base and appends — same precedence rule
   `vm::resolve_env` uses for `airlock.toml` over image env.
4. The merged env is forwarded to `Supervisor.exec`, which hands
   the full env to the child.

Schema stays unchanged — `CliService.exec.env` just shifts meaning
from "full env" to "overrides". `Supervisor.exec.env` is still the
complete list, because the supervisor does `env_clear()` + `envs()`
on the child.

## Why the CLI-server-side merge

- `airlock exec` no longer needs to know the image, the vault, or
  anything about config resolution. It's a socket client.
- The base env is resolved exactly once — at `airlock start` — and
  reused for every exec, instead of each exec re-running
  `vm::resolve_env` with a fresh vault unlock.
- The `VmInstance.env` that the main container process was launched
  with *is* the authoritative environment for the running sandbox.
  Exec'd processes inheriting that env matches user expectation
  ("start a shell inside my running sandbox with the same vars").

## Dropped

- `cmd_exec::resolve_config_env` — the whole per-exec config/vault
  pass.
- `project::load(vault)` in `cmd_exec`. No vault prompt on exec.
- The `vault: Vault` parameter on `cmd_exec::main`.
- Default-cwd-from-run.json lookup. `run.json`'s `guest_cwd` field
  stays (still used for the `airlock show` display and as the start
  cwd override) but exec no longer reads it — every exec uses the
  caller's actual current directory.

## File changes

- `app/airlock-cli/src/cli/cmd_exec.rs` — rewrite without project
  load, adds `find_cli_sock` walk-up.
- `app/airlock-cli/src/cli_server.rs` — `serve` takes `base_env`,
  new `merge_env` helper, unit tests for override semantics.
- `app/airlock-cli/src/cli/cmd_start.rs` — passes `vm.env.clone()`
  to `cli_server::serve`.
- `app/airlock-cli/src/main.rs` — stop threading `vault` into
  `cmd_exec::main`.
- `docs/manual/src/usage/attaching-to-running-sandbox.md` — update
  the working-directory and env-override sections to match the new
  behavior.
