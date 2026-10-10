//! Ways to run async test code to completion, and to wait until a state
//! that comes later is true.

use std::future::Future;
use std::time::{Duration, Instant};

use tokio::task::LocalSet;

/// Run `fut` to completion on a new current-thread runtime in a
/// `LocalSet`, as the binaries do. Then `spawn_local` and Cap'n Proto work.
pub fn block_on_local<F: Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    LocalSet::new().block_on(&rt, fut)
}

/// Run `fut` to completion on the current thread, without a runtime (for
/// async code that does no I/O).
pub fn block_on<F: Future>(fut: F) -> F::Output {
    futures::executor::block_on(fut)
}

/// Maximum time that [`eventually`] and [`eventually_async`] wait.
const EVENTUALLY_LIMIT: Duration = Duration::from_secs(10);

/// Time between two tries of [`eventually`] and [`eventually_async`].
const EVENTUALLY_STEP: Duration = Duration::from_millis(10);

/// Call `f` until it gives a value. Panics after 10 seconds.
///
/// Use it to wait for a state that comes later. Also use it when a test
/// releases an OS resource (a lock, a port or a socket) and then uses it
/// again. Tests run in parallel, and a `fork` in a different test copies
/// all open files of the process. The child keeps them until its `exec`,
/// so the release can come late.
/// Args:
///  - `what`: Description of the state, for the panic message
///  - `f`: Check that gives `Some` when the state is true
pub fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + EVENTUALLY_LIMIT;
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(EVENTUALLY_STEP);
    }
}

/// Async [`eventually`] with an async check. Sleeps on the runtime between
/// the tries, so that the other tasks of the test can run.
pub async fn eventually_async<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + EVENTUALLY_LIMIT;
    loop {
        if let Some(value) = f().await {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(EVENTUALLY_STEP).await;
    }
}
