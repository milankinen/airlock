use ezpez_protocol::supervisor_capnp::*;

pub enum ProcessEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
}

#[derive(Clone)]
pub struct Process {
    proc: process::Client,
}

impl Process {
    pub fn new(proc: process::Client) -> Self {
        Self { proc }
    }

    pub async fn signal(&self, signum: i32) -> anyhow::Result<()> {
        let mut req = self.proc.signal_request();
        req.get().set_signum(signum);
        req.send().promise.await?;
        Ok(())
    }

    pub async fn poll(&self) -> anyhow::Result<ProcessEvent> {
        let response = self.proc.poll_request().send().promise.await?;
        let next = response.get()?.get_next()?;

        match next.which() {
            Ok(process_output::Exit(code)) => Ok(ProcessEvent::Exit(code)),
            Ok(process_output::Stdout(frame)) => {
                let frame = frame?;
                match frame.which() {
                    Ok(data_frame::Data(Ok(data))) => Ok(ProcessEvent::Stdout(data.to_vec())),
                    Ok(data_frame::Eof(())) => Ok(ProcessEvent::Exit(0)),
                    _ => Ok(ProcessEvent::Exit(1)),
                }
            }
            Ok(process_output::Stderr(frame)) => {
                let frame = frame?;
                match frame.which() {
                    Ok(data_frame::Data(Ok(data))) => Ok(ProcessEvent::Stderr(data.to_vec())),
                    _ => Ok(ProcessEvent::Exit(1)),
                }
            }
            Err(_) => Ok(ProcessEvent::Exit(1)),
        }
    }
}
