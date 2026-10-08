//! Ownership of every background task a boot starts.

use std::future::Future;

use tokio::task::JoinSet;

/// The background tasks of one boot, in two groups that stop at different
/// points of the shutdown:
///
/// - **services** use the RPC connections: clock sync, deny reporter,
///   reverse port forwards, the `airlock exec` server and the signal
///   forwarder. They stop first, before the guest is asked to sync, so
///   their ports and sockets are free when the shutdown returns.
/// - **transport** runs the supervisor and network RPC connections. It
///   stops last, after the VM, so the guest stays reachable until then.
///
/// Dropping the value aborts both groups.
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

    /// The service set, for helpers that spawn several tasks into it.
    pub fn services(&mut self) -> &mut JoinSet<()> {
        &mut self.services
    }

    /// Run `task` as a transport task.
    pub fn spawn_transport(&mut self, task: impl Future<Output = ()> + 'static) {
        self.transport.spawn_local(task);
    }

    /// Abort every service and wait until each one is dropped.
    pub async fn stop_services(&mut self) {
        stop(&mut self.services).await;
    }

    /// Abort every transport task and wait until each one is dropped.
    pub async fn stop_transport(&mut self) {
        stop(&mut self.transport).await;
    }
}

/// Abort all tasks in `set` and wait for them. A task's future (and so
/// every listener or socket it owns) is dropped before `join_next` reports
/// it, so the resources are free when this returns.
async fn stop(set: &mut JoinSet<()>) {
    set.abort_all();
    while set.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::rc::Rc;

    use tokio::net::TcpListener;

    use super::*;
    use crate::test_cfg::block_on_local;

    /// Sets its flag when dropped, i.e. when the owning task is aborted.
    struct DropFlag(Rc<Cell<bool>>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

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

    #[test]
    fn services_stop_before_transport_and_free_their_ports() {
        block_on_local(async {
            let mut tasks = BootTasks::default();
            let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let service_dropped = Rc::new(Cell::new(false));
            let flag = DropFlag(service_dropped.clone());
            tasks.spawn_service(async move {
                let _flag = flag;
                loop {
                    let _ = listener.accept().await;
                }
            });
            let transport_dropped = spawn_pending(|task| tasks.spawn_transport(task));
            tokio::task::yield_now().await;

            tasks.stop_services().await;
            assert!(service_dropped.get());
            assert!(!transport_dropped.get());
            TcpListener::bind(addr).await.unwrap();

            tasks.stop_transport().await;
            assert!(transport_dropped.get());
        });
    }

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
