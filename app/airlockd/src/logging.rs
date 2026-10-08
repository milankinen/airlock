//! Guest logging.
//!
//! Sends the log events of the guest to the host CLI. The guest cannot write
//! to a terminal or to a log file directly, so the host shows the guest logs.
//! The host also sets the log level.

use std::fmt::Write;

use airlock_common::supervisor_capnp::log_sink;
use tokio::sync::mpsc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Install the global tracing subscriber that sends events to the host.
/// Args:
///  - `log_sink`: Host `LogSink` capability that receives the events
///  - `log_filter`: `EnvFilter` directive string, for example `info`
pub fn init(log_sink: log_sink::Client, log_filter: &str) {
    let (tx, rx) = mpsc::unbounded_channel::<(u8, String)>();
    let filter = EnvFilter::new(log_filter);
    tracing_subscriber::registry()
        .with(RpcLayer { tx }.with_filter(filter))
        .init();

    tokio::task::spawn_local(forward(log_sink, rx));
}

/// Read events from the channel and send each event to the host with RPC.
async fn forward(log_sink: log_sink::Client, mut rx: mpsc::UnboundedReceiver<(u8, String)>) {
    while let Some((level, msg)) = rx.recv().await {
        let mut req = log_sink.log_request();
        req.get().set_level(level);
        req.get().set_message(&msg);
        drop(req.send());
    }
}

/// Tracing layer that puts each log event as a `(level, message)` pair in a
/// queue for [`forward`].
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
            tracing::Level::ERROR => 4,
            tracing::Level::WARN => 3,
            tracing::Level::INFO => 2,
            tracing::Level::DEBUG => 1,
            tracing::Level::TRACE => 0,
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
