//! Host-side wrapper around a guest `Process` RPC capability.

use airlock_common::supervisor_capnp::*;

/// A decoded output event from a guest process.
pub enum ProcessEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
}

/// Typed wrapper around the Cap'n Proto `Process` client capability.
#[derive(Clone)]
pub struct Process {
    proc: process::Client,
}

impl Process {
    /// Wrap a raw Cap'n Proto process client.
    pub fn new(proc: process::Client) -> Self {
        Self { proc }
    }

    /// Send a Unix signal to the guest process.
    pub async fn signal(&self, signum: i32) -> anyhow::Result<()> {
        let mut req = self.proc.signal_request();
        req.get().set_signum(signum);
        req.send().promise.await?;
        Ok(())
    }

    /// Poll for the next output event (stdout chunk, stderr chunk, or exit).
    ///
    /// A stream `Eof` only marks the end of a stdout/stderr stream, not the
    /// process exit, so it is skipped: we keep polling until the guest delivers
    /// the real `exit` event. This is what stops a stdout EOF from masking the
    /// true exit code. Malformed or unknown frames are logged and surfaced as
    /// an error instead of being silently reported as `Exit(1)`.
    pub async fn poll(&self) -> anyhow::Result<ProcessEvent> {
        loop {
            let response = self.proc.poll_request().send().promise.await?;
            let next = response.get()?.get_next()?;

            match next.which() {
                Ok(process_output::Exit(code)) => return Ok(ProcessEvent::Exit(code)),
                Ok(process_output::Stdout(frame)) => {
                    let frame = frame?;
                    match frame.which() {
                        Ok(data_frame::Data(Ok(data))) => {
                            return Ok(ProcessEvent::Stdout(data.to_vec()));
                        }
                        Ok(data_frame::Eof(())) => {}
                        Ok(data_frame::Data(Err(e))) => {
                            tracing::error!("guest stdout frame decode failed: {e}");
                            anyhow::bail!("guest stdout frame decode failed: {e}");
                        }
                        Err(e) => {
                            tracing::error!("unknown guest stdout frame (schema skew?): {e}");
                            anyhow::bail!("unknown guest stdout frame: {e}");
                        }
                    }
                }
                Ok(process_output::Stderr(frame)) => {
                    let frame = frame?;
                    match frame.which() {
                        Ok(data_frame::Data(Ok(data))) => {
                            return Ok(ProcessEvent::Stderr(data.to_vec()));
                        }
                        Ok(data_frame::Eof(())) => {}
                        Ok(data_frame::Data(Err(e))) => {
                            tracing::error!("guest stderr frame decode failed: {e}");
                            anyhow::bail!("guest stderr frame decode failed: {e}");
                        }
                        Err(e) => {
                            tracing::error!("unknown guest stderr frame (schema skew?): {e}");
                            anyhow::bail!("unknown guest stderr frame: {e}");
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("unknown guest process output (schema skew?): {e}");
                    anyhow::bail!("unknown guest process output: {e}");
                }
            }
        }
    }
}
