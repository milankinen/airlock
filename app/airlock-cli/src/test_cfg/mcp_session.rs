//! Fake guest side of host MCP sessions: a sink that records the server
//! messages, and helpers to open sessions and send messages.

use std::rc::Rc;
use std::time::Duration;

use airlock_common::supervisor_capnp::{mcp_host, mcp_session, mcp_sink};
use tokio::sync::mpsc;

/// What the guest sink got.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Message(String),
    Closed(String),
}

/// Guest sink that sends each call to a channel.
struct Sink {
    events: mpsc::UnboundedSender<Event>,
    /// Fail each `message` call, as a guest that is gone.
    fail: bool,
}

impl mcp_sink::Server for Sink {
    async fn message(
        self: Rc<Self>,
        params: mcp_sink::MessageParams,
        _results: mcp_sink::MessageResults,
    ) -> Result<(), capnp::Error> {
        let message = params.get()?.get_message()?.to_str()?.to_string();
        let _ = self.events.send(Event::Message(message));
        if self.fail {
            return Err(capnp::Error::failed("guest gone".into()));
        }
        Ok(())
    }

    async fn closed(
        self: Rc<Self>,
        params: mcp_sink::ClosedParams,
        _results: mcp_sink::ClosedResults,
    ) -> Result<(), capnp::Error> {
        let reason = params.get()?.get_reason()?.to_str()?.to_string();
        let _ = self.events.send(Event::Closed(reason));
        Ok(())
    }
}

/// Open a session of the server `name`. With `fail`, the sink fails each
/// message call.
/// Returns:
///   The session and the receiver of the sink events, or the RPC error
///   message.
pub async fn open(
    host: &mcp_host::Client,
    name: &str,
    fail: bool,
) -> Result<(mcp_session::Client, mpsc::UnboundedReceiver<Event>), String> {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut request = host.open_request();
    request.get().set_name(name);
    request
        .get()
        .set_sink(capnp_rpc::new_client::<mcp_sink::Client, _>(Sink {
            events: tx,
            fail,
        }));
    let response = request.send().promise.await.map_err(|e| e.to_string())?;
    Ok((response.get().unwrap().get_session().unwrap(), rx))
}

/// Send `message` to the server. Returns the RPC error message on failure.
pub async fn send(session: &mcp_session::Client, message: &str) -> Result<(), String> {
    let mut request = session.send_request();
    request.get().set_message(message);
    request
        .send()
        .promise
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The next sink event. Panics after 10 seconds without one.
pub async fn next(events: &mut mpsc::UnboundedReceiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("sink event in time")
        .expect("open sink")
}

/// The next sink event as a JSON message. Panics on a close.
pub async fn next_json(events: &mut mpsc::UnboundedReceiver<Event>) -> serde_json::Value {
    match next(events).await {
        Event::Message(message) => serde_json::from_str(&message).unwrap(),
        Event::Closed(reason) => panic!("closed: {reason}"),
    }
}
