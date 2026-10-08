//! Guest log forwarding.
//!
//! Receives log events from the supervisor in the VM and writes them to the
//! host log.

use std::rc::Rc;

use airlock_common::supervisor_capnp::log_sink;

/// Cap'n Proto `LogSink` server that sends guest log events to the host
/// tracing system.
pub struct LogSinkImpl;

impl log_sink::Server for LogSinkImpl {
    async fn log(self: Rc<Self>, params: log_sink::LogParams) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let level = params.get_level();
        let message = params.get_message()?.to_str()?;

        match level {
            1 => tracing::debug!(target: "airlock::airlockd", "{message}"),
            2 => tracing::info!(target: "airlock::airlockd", "{message}"),
            3 => tracing::warn!(target: "airlock::airlockd", "{message}"),
            4 => tracing::error!(target: "airlock::airlockd", "{message}"),
            _ => tracing::trace!(target: "airlock::airlockd", "{message}"),
        }
        Ok(())
    }
}
