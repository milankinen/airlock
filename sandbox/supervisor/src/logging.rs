use ezpez_protocol::supervisor_capnp::log_sink;
use std::fmt::Write;
use tokio::sync::mpsc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

pub fn init(log_sink: log_sink::Client) {
    let (tx, rx) = mpsc::unbounded_channel::<(u8, String)>();

    tracing_subscriber::registry()
        .with(RpcLayer(tx))
        .init();

    tokio::task::spawn_local(forward(log_sink, rx));
}

async fn forward(log_sink: log_sink::Client, mut rx: mpsc::UnboundedReceiver<(u8, String)>) {
    while let Some((level, msg)) = rx.recv().await {
        let mut req = log_sink.log_request();
        req.get().set_level(level);
        req.get().set_message(&msg);
        let _ = req.send();
    }
}

struct RpcLayer(mpsc::UnboundedSender<(u8, String)>);

impl<S: tracing::Subscriber> Layer<S> for RpcLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let level = match *event.metadata().level() {
            tracing::Level::ERROR => 3,
            tracing::Level::WARN => 2,
            tracing::Level::INFO => 1,
            tracing::Level::DEBUG => 0,
            tracing::Level::TRACE => 0,
        };

        let mut visitor = MsgVisitor(String::new());
        event.record(&mut visitor);
        let _ = self.0.send((level, visitor.0));
    }
}

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
