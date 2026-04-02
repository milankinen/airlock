use ezpez_protocol::supervisor_capnp::log_sink;
use std::rc::Rc;

pub struct LogSinkImpl;

impl log_sink::Server for LogSinkImpl {
    async fn log(
        self: Rc<Self>,
        params: log_sink::LogParams,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let level = params.get_level();
        let message = params.get_message()?.to_str()?;

        match level {
            0 => tracing::debug!(target: "vm", "{message}"),
            1 => tracing::info!(target: "vm", "{message}"),
            2 => tracing::warn!(target: "vm", "{message}"),
            _ => tracing::error!(target: "vm", "{message}"),
        }
        Ok(())
    }
}
