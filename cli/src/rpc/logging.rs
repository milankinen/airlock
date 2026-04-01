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
            0 => eprintln!("  [vm] {message}"),
            1 => eprintln!("  [vm] {message}"),
            2 => eprintln!("  [vm] WARN: {message}"),
            _ => eprintln!("  [vm] ERROR: {message}"),
        }
        Ok(())
    }
}
