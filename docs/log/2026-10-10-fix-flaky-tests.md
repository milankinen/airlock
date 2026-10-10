# Fix flaky tests

CI failed once on `lock_is_exclusive_and_is_running_tracks_it` with "idle
lock is not taken". An audit of the suite for concurrency and timing
problems found more tests with the same root cause, and a few with other
timing assumptions.

## Root cause: fork copies open descriptors

`cargo test` runs the tests of a crate as threads in one process. Some
tests start child processes (shims, setup scripts, `sh`). A `fork` copies
every open descriptor of the process, and the child keeps the copies until
its `exec` closes the `O_CLOEXEC` ones. So for a few milliseconds:

- a released `flock` sandbox lock is still held (it belongs to the open
  file description, which the child shares)
- a closed TCP listener still holds its port
- a closed Unix listener still accepts connections

A test that releases one of these and at once expects it to be free fails
from time to time. The production CLI does not fork while it releases and
takes a lock again, so this is a test-only problem there.

## Fixes

- `airlock-test-utils` has `eventually` and `eventually_async` (10 s
  limit). The async variant takes an async closure, so it can poll async
  state such as the grant store. airlockd had its own copy, which is gone.
- Lock tests, `rm` tests, the remove-box test and the sandbox location
  tests retry after a release. The setup helpers make a stopped sandbox
  (directory and empty `lock` file) without a lock, so no descriptor
  exists to copy. The start helpers also wait until the project sandbox
  is not running: else the move step sees a held lock, skips the move
  silently, and the test fails later. Their old retry was 200 x 5 ms.
- Sign-in, callback forward and Unix socket tests wait for the port or
  socket with a deadline instead of 50 `yield_now` calls or no retry.
- The revoke-dropped-by-guest test dropped the guest request after a fixed
  100 ms, which could be before the revoke reached the fake provider. The
  fake provider now has a revoke gate (the refresh gate became a general
  `RequestGate`). The test drops the request when the revoke arrives. The
  `revoke_delay` option is gone. The revoke wait helpers panic on timeout
  instead of returning silently.
- The adversarial scan test limit is 10 s, not 1 s. Loaded CI machines
  took slightly more than 1 s in a debug build. A quadratic scan is much
  slower than 10 s, so the test still finds it.
- The keep-alive relay tests read until the full response arrives, not a
  fixed 500 ms. The shared stream read helpers allow 10 s, not 2 s.
- The VM network bats test waits until the host HTTP server accepts
  connections instead of `sleep 2`.

## Production changes

- `LoopbackSignIn::detach` promised that the callback port is free when it
  returns. It aborted the outer forward task. That dropped the set of
  accept loops, which only marks those tasks cancelled. Their listeners
  drop at a later poll. Now the forward has an `end` signal. Its task then
  shuts down the accept loops with `JoinSet::shutdown` and ends after the
  listeners are gone. `detach` ends the forward and joins the task. A
  replaced forward is ended and its task detached, so it frees its port
  without an abort.
- airlockd copy and browser bridges: two quick FIFO writes could join one
  read cycle and mix into one copy or one open (seen in tests as dropped
  "over limit" payloads). `read_capped` now renews the FIFO right after
  the open, as the paste loop already did. A later writer then opens the
  new FIFO and waits for the next cycle. This makes the window smaller but
  does not remove it: a writer can write, close, and let the next writer
  open the old FIFO before the reader renames it. Tests that write
  directly to the FIFO use `write_fifo_cycle`, which waits until the FIFO
  inode changes. Two writers that open at the same time still share one
  cycle.

## Not fixed (low likelihood)

- `free_port_in` picks a port with bind-and-drop. Another test can take
  the port before the sign-in binds it.
- `rm` and `sandboxes rm` call `is_running` before `remove_box` takes the
  lock. A fork during that short probe can make the second check see a
  held lock.
- The clipboard tests run shims back to back with no wait for the FIFO
  renew. Process start is slow enough that they did not fail in 100
  loaded runs.
- Callback and auth-code tests use `Instant::now().checked_sub(10 min)`,
  which panics on a machine with less than 10 minutes of uptime. A fix
  needs an injected clock.

## Verification

20 full `cargo test --workspace` runs with 30 CPU-burning loops on a
10-core machine, with no failures. Before the fixes, an idle 20-run loop
failed once, and loaded runs failed the scan test 3 of 5 times. The
airlockd bridge tests passed 100 of 100 loaded runs.
