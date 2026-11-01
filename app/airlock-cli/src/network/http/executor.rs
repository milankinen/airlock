//! Task runner for HTTP connections on the current thread.
//!
//! The HTTP stack starts helper tasks for each connection. A task group
//! ties these tasks to the connection. When the connection task stops or
//! is aborted, all helper tasks of the connection stop too.

use std::future::Future;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

tokio::task_local! {
    /// Stop signal of the connection that the current task serves.
    static GROUP: CancellationToken;
}

/// Run `fut` in a new task group. Tasks that [`spawn`] starts inside `fut`
/// belong to the group. When `fut` completes or is dropped, all tasks of
/// the group stop.
pub async fn scope<F: Future>(fut: F) -> F::Output {
    let token = CancellationToken::new();
    // The guard is part of the returned future. Thus an abort of the
    // connection task drops it and stops the group.
    let _stop = token.clone().drop_guard();
    GROUP.scope(token, fut).await
}

/// Start `fut` on the current `LocalSet`. Inside a [`scope`], the task
/// joins the group, and its own new tasks join it too.
/// Returns:
///   The task handle. When the group stops, the task stops at once with
///   the default output. Nobody reads this output, because the owner of
///   the group is gone.
pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: Default + 'static,
{
    let Ok(token) = GROUP.try_with(CancellationToken::clone) else {
        return tokio::task::spawn_local(fut);
    };
    tokio::task::spawn_local(GROUP.scope(token.clone(), async move {
        token.run_until_cancelled(fut).await.unwrap_or_default()
    }))
}

/// hyper executor that starts futures on the current `LocalSet`, in the
/// task group of the connection (see [`spawn`]).
#[derive(Clone)]
pub struct LocalExecutor;

impl<F> hyper::rt::Executor<F> for LocalExecutor
where
    F: Future + 'static,
    F::Output: Default + 'static,
{
    fn execute(&self, fut: F) {
        spawn(fut);
    }
}
