# Don't let a stream EOF mask a process's real exit code

## Symptom

A guest command that failed could be reported to the host as having succeeded
(exit 0), and its pending stderr could be dropped. Conversely, a Cap'n Proto
schema-version skew between host and guest surfaced as a plain "exit 1" — an
ordinary command failure — with nothing logged to explain it.

## Root cause

`Process::poll` in `app/airlock-cli/src/rpc/process.rs` returns one output event
per call, and every caller stops its read loop on the first `Exit`. The decoder
translated a stdout stream `Eof` frame into `Exit(0)` and any unknown or
malformed frame into `Exit(1)`. A stream EOF only means "this stdout/stderr
stream is closed", not "the process exited", so mapping it to `Exit(0)` ended
the loop early — before the real `exit` event (with the true code) and any
buffered stderr were ever polled. The catch-all `Exit(1)` arms did the same for
decode errors and out-of-schema union tags (exactly what a schema skew looks
like), silently manufacturing an exit code and logging nothing.

## Fix

`poll` is now a loop. A stdout or stderr `Eof` frame is skipped (`continue`) and
polling resumes until the guest delivers the actual `exit` event, so the true
exit code is always the one reported. Data-pointer decode failures and unknown
`DataFrame` / `ProcessOutput` union tags are logged at `error` and returned as
an error, instead of being silently coerced into `Exit(1)`. The three callers
(`airlock exec`, `airlock start`, and the CLI RPC bridge) are unchanged: they
already terminate on `Exit` and log/exit on a poll error.

The live guest never emits an `eof` frame today and always sends `exit` last, so
this is primarily a protocol-correctness/defensive fix; skipping EOF cannot spin
against the current guest because the loop always reaches `exit`.

## Tests

No isolated unit test: `poll` is bound to a live Cap'n Proto `process::Client`,
so reproducing the EOF and malformed-frame paths needs a mock RPC server and a
Tokio `LocalSet` — an integration test. Exit-code reporting is covered by the VM
bats suite (`bats:vm`, needs KVM + Docker), not run here.
