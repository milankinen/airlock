//! Receives log events from the in-VM supervisor and re-emits them through
//! the host's `tracing` subscriber under the `airlock::airlockd` target.

use std::rc::Rc;

use airlock_protocol::supervisor_capnp::log_sink;

/// Cap'n Proto `LogSink` server that bridges guest log events into the host
/// tracing system.
pub struct LogSinkImpl;

impl log_sink::Server for LogSinkImpl {
    async fn log(self: Rc<Self>, params: log_sink::LogParams) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let level = params.get_level();
        let message = params.get_message()?.to_str()?;

        match level {
            0 => tracing::debug!(target: "airlock::airlockd", "{message}"),
            1 => tracing::info!(target: "airlock::airlockd", "{message}"),
            2 => tracing::warn!(target: "airlock::airlockd", "{message}"),
            _ => tracing::error!(target: "airlock::airlockd", "{message}"),
        }
        Ok(())
    }
}
