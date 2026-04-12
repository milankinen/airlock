//! Forwards `tracing` log events from the guest to the host CLI over RPC.
//!
//! The guest has no direct access to stderr or a log file. Instead, a custom
//! `tracing` layer serialises every event into a `(level, message)` pair and
//! sends it through the Cap'n Proto `LogSink` interface so the host can
//! display or filter it.

use std::fmt::Write;

use airlock_protocol::supervisor_capnp::log_sink;
use tokio::sync::mpsc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Install the global tracing subscriber that forwards events to the host.
pub fn init(log_sink: log_sink::Client, log_filter: &str) {
    let (tx, rx) = mpsc::unbounded_channel::<(u8, String)>();
    let filter = EnvFilter::new(log_filter);
    tracing_subscriber::registry()
        .with(RpcLayer { tx }.with_filter(filter))
        .init();

    tokio::task::spawn_local(forward(log_sink, rx));
}

/// Drain the channel and send each event to the host via RPC streaming.
async fn forward(log_sink: log_sink::Client, mut rx: mpsc::UnboundedReceiver<(u8, String)>) {
    while let Some((level, msg)) = rx.recv().await {
        let mut req = log_sink.log_request();
        req.get().set_level(level);
        req.get().set_message(&msg);
        drop(req.send());
    }
}

/// Tracing layer that enqueues log events to send over RPC.
struct RpcLayer {
    tx: mpsc::UnboundedSender<(u8, String)>,
}

impl<S: tracing::Subscriber> Layer<S> for RpcLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let level = match *event.metadata().level() {
            tracing::Level::ERROR => 3,
            tracing::Level::WARN => 2,
            tracing::Level::INFO => 1,
            tracing::Level::DEBUG | tracing::Level::TRACE => 0,
        };

        let mut visitor = MsgVisitor(String::new());
        event.record(&mut visitor);
        let _ = self.tx.send((level, visitor.0));
    }
}

/// Collects tracing event fields into a single log message string.
struct MsgVisitor(String);

impl tracing::field::Visit for MsgVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            write!(self.0, "{value:?}").ok();
        } else {
            write!(self.0, " {field}={value:?}").ok();
        }
    }
}
