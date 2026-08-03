# Fix CLI socket server dying on transient accept errors

## Symptom

After a sandbox had been running for a while, `airlock exec` would start
failing with "no running sandbox" even though the VM was healthy and still
running. Once it started, the failure was permanent for that VM — every
subsequent `airlock exec` failed the same way.

## Root cause

The Unix-socket accept loop in `app/airlock-cli/src/cli_server.rs` (`serve`)
`break`s out of the loop on *any* `accept()` error. Transient conditions —
`ECONNABORTED`, or file-descriptor exhaustion such as `EMFILE`/`ENFILE` under
load — therefore ended the server. Ending `serve` drops its `SockGuard`, whose
`Drop` impl unlinks `cli.sock`. With the socket gone but the VM still up, every
later `airlock exec` had nothing to connect to and reported no running sandbox.

## Fix

The error arm of the accept loop no longer breaks. There is no `accept()` error
that justifies unlinking the socket while the VM is alive, so the loop now logs
the error at `warn` and continues serving. A brief 100ms `tokio::time::sleep`
backoff is added on the error path so a persistent condition (e.g. sustained
`EMFILE`) can't hot-spin the loop. The happy path is unchanged, so normal
connection handling has identical behavior. The loop still terminates the
normal way: the server future is cancelled when the VM shuts down.

## Tests

No unit test was added. `serve` binds a real `UnixListener` and loops
indefinitely, and inducing an `accept()` error requires real fd exhaustion or a
fault-injecting acceptor — neither is reachable without refactoring `serve` to
take an injectable acceptor, which is out of scope for this minimal fix. The
change is behavior-preserving on the happy path (the success arm is unchanged),
and existing `merge_env` unit tests continue to cover the surrounding module.
