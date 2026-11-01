//! Deny notifications to the guest.
//!
//! Tells the guest about denied connections. Tools in the sandbox can then see
//! a policy block with no round trip to the host, and the agent can ask for
//! temporary access to the denied hosts.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use airlock_common::supervisor_capnp::supervisor;
use tokio::sync::mpsc;

/// Maximum number of deny entries that wait for the guest. When the queue is
/// full, new entries are dropped. The guest keeps only a short history.
const MAX_PENDING: usize = 256;

/// Maximum length of a host in an entry (the DNS name limit).
const MAX_HOST_CHARS: usize = 253;

/// Time in which a repeat of the last entry is dropped. A tool that
/// retries in a loop then does not fill the queue.
const REPEAT_WINDOW_MS: u64 = 1000;

/// One denied request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DenyEntry {
    /// Unix time in milliseconds.
    pub epoch: u64,
    /// Destination host (or guest socket path).
    pub host: String,
    /// Destination port (0 for sockets).
    pub port: u16,
    /// True when the default policy denied the request, so a temporary grant
    /// can allow it.
    pub grantable: bool,
}

/// Deny notifier that connects to the guest late.
///
/// [`report`](Self::report) puts an entry in a bounded queue. The task from
/// [`attach`](Self::attach) sends the queued entries to the guest in batches
/// with a one-way `Supervisor.reportDeny` RPC.
///
/// Before the task is attached, `report()` does nothing. This is correct
/// for tests (no supervisor) and for the time before boot.
pub struct DenyReporter {
    tx: RefCell<Option<mpsc::Sender<DenyEntry>>>,
    /// Last queued entry.
    last: RefCell<Option<DenyEntry>>,
}

impl DenyReporter {
    /// Make a notifier that is not attached.
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            tx: RefCell::new(None),
            last: RefCell::new(None),
        })
    }

    /// Get the task that sends deny reports to `client`.
    /// The caller owns the task. The task stops when the network is
    /// dropped. Reports from before this call are not sent.
    pub fn attach(&self, client: supervisor::Client) -> impl Future<Output = ()> + 'static {
        let (tx, mut rx) = mpsc::channel(MAX_PENDING);
        *self.tx.borrow_mut() = Some(tx);
        async move {
            while let Some(first) = rx.recv().await {
                // Send all entries that wait now in one RPC.
                let mut batch = vec![first];
                while let Ok(e) = rx.try_recv() {
                    batch.push(e);
                }
                let mut req = client.report_deny_request();
                let mut list = req.get().init_denies(batch.len() as u32);
                for (i, e) in batch.iter().enumerate() {
                    let mut b = list.reborrow().get(i as u32);
                    b.set_epoch(e.epoch);
                    b.set_host(&e.host);
                    b.set_port(e.port);
                    b.set_grantable(e.grantable);
                }
                if let Err(e) = req.send().promise.await {
                    tracing::debug!("report_deny: {e}");
                }
            }
        }
    }

    /// Record a deny now. This never blocks or fails. A deny notification
    /// that does not get to the guest is a visibility problem, not a
    /// correctness problem. It must not block or stop the deny path. When
    /// the queue is full, the entry is dropped. A repeat of the last entry
    /// within one second is also dropped.
    /// Args:
    ///  - `host`: Destination host, or the guest socket path
    ///  - `port`: Destination port, 0 for sockets
    ///  - `grantable`: True when the default policy denied the request
    pub fn report(&self, host: &str, port: u16, grantable: bool) {
        let Some(tx) = self.tx.borrow().clone() else {
            return;
        };
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        // The host comes from the guest. Keep it short and printable.
        let host: String = host
            .chars()
            .filter(|c| !c.is_control() && !is_invisible(*c))
            .take(MAX_HOST_CHARS)
            .collect();
        let entry = DenyEntry {
            epoch,
            host,
            port,
            grantable,
        };
        let mut last = self.last.borrow_mut();
        let repeat = last.as_ref().is_some_and(|l| {
            l.host == entry.host
                && l.port == entry.port
                && l.grantable == entry.grantable
                && entry.epoch.saturating_sub(l.epoch) < REPEAT_WINDOW_MS
        });
        if repeat {
            return;
        }
        if tx.try_send(entry.clone()).is_ok() {
            *last = Some(entry);
        }
    }
}

/// Return true if `c` is an invisible format character: a bidi control, a
/// zero-width character, the byte order mark or a tag character. They can
/// hide or reorder text when a host shows to the user.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{E0000}'..='\u{E007F}'
    )
}
