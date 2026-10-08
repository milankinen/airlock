//! Ways to run async test code to completion.

use std::future::Future;

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
