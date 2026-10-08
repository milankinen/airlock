# Rework the test suites

The Rust and bats test suites had grown many small tests that checked
trivial things (constants, getters, string templates) or behaviour that
another test already covered. Setup code was copied from file to file.
This change rewrites the suites of all crates and the bats tests with
one rule set, which is now in `CLAUDE.md` ("Tests").

Result: Rust 709 → 380 tests (+ 2 ignored), bats cli 101 → 43, bats vm
47 → 23. Coverage of real behaviour is kept. Security checks (secret
masking, deny rules, public-only dialing, path escapes, symlinks, token
swapping, size caps) stay covered, mostly through end-to-end flows now.

## Rules

- Do not test trivial things or removed features.
- Prefer end-to-end tests of a full sub-system flow with real data:
  real config text, real local (TLS) servers, real tar layers, real
  FIFOs and shell scripts. Fake only the edges: the sandbox transport
  (in-memory Cap'n Proto), the VM exec, host programs.
- End-to-end tests live in `<subsystem>/tests/test_<flow>.rs`. Unit
  tests live in an inline `#[cfg(test)] mod tests`.
- Names: `doing_something_with_some_condition_has_some_effect`.
- No comments on tests or in test bodies.

## Shared helpers

- `app/airlock-test-utils`: a new crate, used as a dev-dependency only.
  Helper groups that pull dependencies are behind features (`http`,
  `tls`, `rpc`): temp dirs, `block_on_local`, local HTTP and TLS
  servers, `TestCa`, `tls_trusting`, `read_until_contains`, and
  `rpc_loopback` (a two-party RPC connection over an in-memory duplex).
- `airlock-cli/src/test_cfg/`: replaces `test_support.rs` and the
  per-file helpers of `network/tests`. One file per part: `config`
  (`ConfigDirs` loads real user and project files like the CLI does),
  `context`, `home`, `network` (the network harness, now also built
  from `airlock.toml` text through the real `Network::new`), `upstream`
  (fake TLS upstream, upgrade echo, guest HTTPS client), `provider`
  (one fake OAuth provider for Anthropic and OpenAI), `services`,
  `oci` (tar layer builder), `packs`, `start`, `vault`, `sinks`.
- `airlockd/src/test_cfg/` and `airlock-monitor/src/test_cfg/`: the
  FIFO bridge harness (`run_bridge`), the admin router, and a TUI
  harness that sends real key, mouse and paste events through the event
  handler into a ratatui test backend.

## Test seams in production code

No behaviour changes. Changes made for tests:

- Visibility: `Network` fields, `network::{middleware, tls}`,
  `LayeredConfig::load_from`, `cmd_rm::run`, `docker::save_from_stream`,
  browser `RateLimit`/`with_opener`, wizard `Input`/`check_answers`,
  `InstallProgress::message`.
- `#[cfg(test)]` constructors: `ClipboardImpl::with_programs`,
  `packs::load_test_packs` (nested folders in `builtin::fixture`).
- `start/sandbox.rs` and `start/install.rs`: under `cfg(test)` the image
  pull, the image facts check and the install boot are imported from
  `test_cfg::start`. These fakes call the real functions unless a test
  turns them on.
- airlockd: `admin::server::router`, `bridge::make_fifo_at`, and the
  clipboard and browser loops take the FIFO path as a parameter.
- airlock-monitor: `handle_event` and `handle_mouse` take any ratatui
  backend.
- Removed the unused test-only `InjectedSecret::ptr_eq`.

## New coverage

- A refresh that finishes after a sign-out revokes its new tokens
  upstream (`network/tests/test_refresh_racing_sign_out.rs`). The fake
  provider can hold a refresh (`FakeProvider::hold_next_refresh`) so
  the sign-out lands while the refresh is in flight. This was
  documented but had no test.
- `pack_conflicts`, `[packs]` in a user file, two distro packs
  conflicting on `vm.image`, a running sandbox refused by `airlock rm`,
  the clipboard over real RPC with real `sh`/`cat`, the airlockd FIFO
  bridges with real shims, and the monitor TUI through real events.

## Bats

- Shared setup moved to `tests/helpers.bash`, `tests/cli/helpers.bash`
  and `tests/vm/helpers.bash`.
- `clipboard.bats` called an `assert_output` helper that does not
  exist, so it could not pass. `show` and `exec` expected old messages.
  Both are fixed.
- Removed tests of removed flags (`--reauth`, `--no-agent-signin`,
  `agents`) and trivial help-text checks. Merged single-command VM
  tests into fewer VM boots.

## Leftovers

- Bug: the airlockd paste loop can serve one paste 2–6 times. The test
  `paste_through_every_tool_name_returns_host_clipboard` shows it and
  is ignored.
- Flaky: `services_stop_before_transport_and_free_their_ports` re-binds
  a port it just freed; a parallel test can take the port first.
