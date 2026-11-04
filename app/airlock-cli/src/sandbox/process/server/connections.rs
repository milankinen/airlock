//! Connection count of the sandbox process.
//!
//! Each client connection holds the sandbox, from its accept until it
//! closes. A client can give up its hold and keep the connection, for
//! example a port forward. The count decides when the sandbox stops:
//!  * when the last connection closes, after at least one connection
//!  * when a client requests a stop
//!  * when no connection comes in a time limit, for example because the
//!    parent process stopped
//!
//! After the stop decision, the server closes new connections at once.
//! The clients then wait for the end of the shutdown and start the sandbox
//! again.
//!
//! The count is for one thread only. Thus a check of the stop state and the
//! change of the count occur together, with no other task between them.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use tokio::sync::Notify;

/// Why the sandbox process stops the sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The last connection closed.
    LastClosed,
    /// No connection came in the time limit.
    NoFirstConnection,
    /// A client requested the stop.
    Requested,
}

/// Count of the connections that hold the sandbox, and the stop state of
/// the sandbox.
#[derive(Default)]
pub struct ConnectionTracker {
    count: Cell<u32>,
    opened_once: Cell<bool>,
    stop: Cell<Option<StopReason>>,
    notify: Notify,
}

impl ConnectionTracker {
    /// Make a tracker with no connections.
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// Count one new connection.
    /// Returns:
    ///   The hold of the connection, or `None` if the sandbox stops. Then
    ///   the caller closes the connection.
    pub fn open(self: &Rc<Self>) -> Option<Hold> {
        // No await between the check and the increment. Thus a connection
        // cannot count after the stop decision.
        if self.stop.get().is_some() {
            return None;
        }
        self.count.set(self.count.get() + 1);
        self.opened_once.set(true);
        Some(Hold {
            tracker: self.clone(),
        })
    }

    /// Stop the sandbox now. Open connections stay counted until the
    /// shutdown closes them.
    pub fn request_stop(&self) {
        self.set_stop(StopReason::Requested);
    }

    /// Get the number of connections that hold the sandbox.
    pub fn connections(&self) -> u32 {
        self.count.get()
    }

    /// Get `true` after the stop decision.
    pub fn is_stopping(&self) -> bool {
        self.stop.get().is_some()
    }

    /// Wait for the stop decision. For one waiter only.
    /// Args:
    ///  - `first_timeout`: Time limit for the first connection, from the
    ///    call
    ///
    /// Returns:
    ///   The reason of the stop.
    pub async fn wait_stop(&self, first_timeout: Duration) -> StopReason {
        let deadline = tokio::time::Instant::now() + first_timeout;
        loop {
            if let Some(reason) = self.stop.get() {
                return reason;
            }
            // `notify_one` keeps a permit when no task waits. Thus a stop
            // between the check and the wait is not lost.
            let notified = self.notify.notified();
            if self.opened_once.get() {
                notified.await;
            } else {
                tokio::select! {
                    () = notified => {}
                    () = tokio::time::sleep_until(deadline) => {
                        // A connection can come at any time before the
                        // deadline. Then the connection count applies.
                        if !self.opened_once.get() {
                            self.set_stop(StopReason::NoFirstConnection);
                        }
                    }
                }
            }
        }
    }

    /// Set the stop decision. The first decision stays.
    fn set_stop(&self, reason: StopReason) {
        if self.stop.get().is_none() {
            self.stop.set(Some(reason));
            self.notify.notify_one();
        }
    }

    /// Remove one connection from the count. When it is the last, stop the
    /// sandbox in the same call, so that no connection can count between
    /// the two steps.
    fn close(&self) {
        let count = self.count.get() - 1;
        self.count.set(count);
        if count == 0 {
            self.set_stop(StopReason::LastClosed);
        }
    }
}

/// The hold of one connection on the sandbox. The drop removes the
/// connection from the count.
pub struct Hold {
    tracker: Rc<ConnectionTracker>,
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.tracker.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_cfg::block_on_local;

    /// Test that the sandbox stops when no connection comes in time, and
    /// that the time limit does not apply after the first connection.
    ///   1. Wait with a short limit and no connection
    ///   2. Check the reason, and that a new connection is refused
    ///   3. With a new tracker, count a connection before the wait
    ///   4. Check that the wait continues after the limit, until the
    ///      connection closes
    #[test]
    fn no_first_connection_in_time_stops() {
        block_on_local(async {
            let tracker = ConnectionTracker::new();
            let reason = tracker.wait_stop(Duration::from_millis(20)).await;
            assert_eq!(reason, StopReason::NoFirstConnection);
            assert!(tracker.open().is_none());

            let tracker = ConnectionTracker::new();
            let hold = tracker.open().unwrap();
            let wait = tracker.wait_stop(Duration::from_millis(20));
            let late_close = async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                drop(hold);
                std::future::pending::<StopReason>().await
            };
            let reason = tokio::select! {
                reason = wait => reason,
                reason = late_close => reason,
            };
            assert_eq!(reason, StopReason::LastClosed);
        });
    }
}
