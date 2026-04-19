/// Executor that spawns futures on the current LocalSet.
#[derive(Clone)]
pub struct LocalExecutor;

impl<F> hyper::rt::Executor<F> for LocalExecutor
where
    F: Future + 'static,
{
    fn execute(&self, fut: F) {
        tokio::task::spawn_local(fut);
    }
}
