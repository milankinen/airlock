//! Background tasks of a boot.
//!
//! Keeps all background tasks that a boot starts, and stops them in order
//! during the shutdown. The tasks stop before the next boot in the same
//! process can start.

use std::future::Future;

use tokio::task::JoinSet;

/// The background tasks of one boot. Two groups stop at different points of
/// the shutdown:
///
/// - **services** use the RPC connections: clock sync, deny reporter,
///   reverse port forwards, the `airlock exec` server and the signal
///   forwarder. They stop first, before the guest gets the sync request.
///   Thus their ports and sockets are free when the shutdown returns.
/// - **transport** runs the supervisor and network RPC connections. It stops
///   last, after the VM, so the guest stays reachable until then.
///
/// A drop of the value aborts both groups.
#[derive(Default)]
pub(crate) struct BootTasks {
    services: JoinSet<()>,
    transport: JoinSet<()>,
}

impl BootTasks {
    /// Run `task` as a service.
    pub fn spawn_service(&mut self, task: impl Future<Output = ()> + 'static) {
        self.services.spawn_local(task);
    }

    /// Get the service set, for helpers that spawn several tasks into it.
    pub fn services(&mut self) -> &mut JoinSet<()> {
        &mut self.services
    }

    /// Run `task` as a transport task.
    pub fn spawn_transport(&mut self, task: impl Future<Output = ()> + 'static) {
        self.transport.spawn_local(task);
    }

    /// Abort all services and wait until each one is dropped.
    pub async fn stop_services(&mut self) {
        stop(&mut self.services).await;
    }

    /// Abort all transport tasks and wait until each one is dropped.
    pub async fn stop_transport(&mut self) {
        stop(&mut self.transport).await;
    }
}

/// Abort all tasks in `set` and wait for them. The future of a task (and all
/// listeners and sockets that it owns) drops before `join_next` reports it.
/// Thus the resources are free when this function returns.
async fn stop(set: &mut JoinSet<()>) {
    set.abort_all();
    while set.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    //! Tests for the stop order and the abort of the boot tasks.

    use std::cell::Cell;
    use std::rc::Rc;

    use tokio::net::{UnixListener, UnixStream};

    use super::*;
    use crate::test_cfg::{block_on_local, temp_dir};

    /// Sets its flag when it drops, which happens when its task stops.
    struct DropFlag(Rc<Cell<bool>>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    /// Spawn a task that never ends, with `spawn`.
    /// Returns:
    ///   A flag that is `true` after the task stops.
    fn spawn_pending(
        spawn: impl FnOnce(std::pin::Pin<Box<dyn Future<Output = ()>>>),
    ) -> Rc<Cell<bool>> {
        let dropped = Rc::new(Cell::new(false));
        let flag = DropFlag(dropped.clone());
        spawn(Box::pin(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        }));
        dropped
    }

    /// Test that the services stop before the transport and that their
    /// listeners are closed after the stop.
    ///   1. Spawn a service that listens on a socket, and a transport task
    ///   2. Stop the services and check that only the service stopped
    ///   3. Check that a connect to the socket is refused
    ///   4. Stop the transport and check that it stopped
    #[test]
    fn services_stop_before_transport_and_close_their_listeners() {
        // A Unix socket in a private directory, not a TCP port. Parallel
        // tests can take a freed TCP port before this test checks it.
        let dir = temp_dir();
        let path = dir.path().join("service.sock");
        block_on_local(async {
            let mut tasks = BootTasks::default();
            let listener = UnixListener::bind(&path).unwrap();
            let service_dropped = Rc::new(Cell::new(false));
            let flag = DropFlag(service_dropped.clone());
            tasks.spawn_service(async move {
                let _flag = flag;
                loop {
                    let _ = listener.accept().await;
                }
            });
            let transport_dropped = spawn_pending(|task| tasks.spawn_transport(task));
            // Let the tasks start, so that the stop aborts running tasks.
            tokio::task::yield_now().await;

            tasks.stop_services().await;
            assert!(service_dropped.get());
            assert!(!transport_dropped.get());
            let err = UnixStream::connect(&path).await.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);

            tasks.stop_transport().await;
            assert!(transport_dropped.get());
        });
    }

    /// Test that a drop of the boot tasks stops the services and the
    /// transport, so that no task is left after a failed boot.
    ///   1. Spawn a service and a transport task
    ///   2. Drop the boot tasks
    ///   3. Check that both tasks stopped
    #[test]
    fn dropping_boot_tasks_aborts_both_groups() {
        block_on_local(async {
            let mut tasks = BootTasks::default();
            let service = spawn_pending(|task| tasks.spawn_service(task));
            let transport = spawn_pending(|task| tasks.spawn_transport(task));
            tokio::task::yield_now().await;

            drop(tasks);
            tokio::task::yield_now().await;

            assert!(service.get() && transport.get());
        });
    }
}
