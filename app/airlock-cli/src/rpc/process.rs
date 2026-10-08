//! Host-side handle to a guest process.
//!
//! Lets the host control a process that runs in the VM and receive its
//! output and exit status.

use airlock_common::supervisor_capnp::*;

/// Decoded output event of a guest process.
pub enum ProcessEvent {
    /// Chunk of stdout data.
    Stdout(Vec<u8>),
    /// Chunk of stderr data.
    Stderr(Vec<u8>),
    /// Process exit with its exit code.
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

    /// Kill the guest process (SIGKILL).
    pub async fn kill(&self) -> anyhow::Result<()> {
        self.proc.kill_request().send().promise.await?;
        Ok(())
    }

    /// Wait for the next output event (stdout chunk, stderr chunk or exit).
    /// Returns:
    ///   Next event, or an error for a malformed or unknown frame. Such a
    ///   frame is logged, and not silently reported as `Exit(1)`.
    pub async fn poll(&self) -> anyhow::Result<ProcessEvent> {
        // A stream `Eof` marks only the end of stdout or stderr, not the
        // process exit. Skip it and poll until the guest sends the real
        // `exit` event. Thus a stdout EOF does not hide the true exit code.
        // Data after an `Eof` of the same stream still goes out. The guest
        // never sends such data, and output must not get lost if it does.
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
