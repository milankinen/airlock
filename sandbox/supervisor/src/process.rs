use ezpez_protocol::supervisor_capnp::*;
use std::cell::RefCell;
use std::fmt::Display;
use std::rc::Rc;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{error, trace};
use crate::rpc::HostProcess;

pub struct SpawnedProcess {
    child: tokio::process::Child,
    pty: Option<pty_process::Pty>,
}

pub fn spawn(cmd: &str, args: &[&str], use_pty: bool) -> Result<SpawnedProcess, anyhow::Error> {
    if use_pty {
        let (pty, pts) = pty_process::open()?;
        let child = pty_process::Command::new(cmd)
            .args(args)
            .env("TERM", "linux")
            .spawn(pts)?;
        Ok(SpawnedProcess { child, pty: Some(pty) })
    } else {
        let child = tokio::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        Ok(SpawnedProcess { child, pty: None })
    }
}

impl SpawnedProcess {
    pub async fn attach(self, host: HostProcess) -> i32 {
        match self.pty {
            Some(pty) => attach_pty(self.child, pty, host).await,
            None => attach_pipe(self.child, host).await,
        }
    }

}

async fn attach_pty(mut child: tokio::process::Child, pty: pty_process::Pty, host: HostProcess) -> i32 {
        use std::os::unix::io::AsRawFd;
        let pty_fd = pty.as_raw_fd();
        let (mut pty_reader, pty_writer) = pty.into_split();

        if let Some((rows, cols)) = host.pty_size {
            if let Err(e) = pty_writer.resize(pty_process::Size::new(rows, cols)) {
                error!("pty resize failure: {e:#}");
            }
        }

        let stdin = host.stdin;
        tokio::task::spawn_local(async move {
            if let Err(e) = relay_stdin_pty(stdin, pty_writer).await {
                error!("stdin failure: {e:#}");
            }
        });

        let (signals_tx, mut signals_rx) = tokio::sync::mpsc::channel(1);
        let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<Frame>(1);
        let _ = host.attachment.send(capnp_rpc::new_client(ProcessImpl {
            frames: RefCell::new(frames_rx),
            signals: RefCell::new(signals_tx),
        }));

        let mut buf = [0u8; 4096];
        loop {
            tokio::select! {
                res = pty_reader.read(&mut buf) => match res {
                    Ok(0) | Err(_) => break,
                    Ok(n) => log_error(frames_tx.send(Frame::Stdout(Bytes::copy_from_slice(&buf[..n]))).await),
                },
                s = signals_rx.recv() => match s {
                    Some(Signal::Num(signum)) => {
                        trace!("signal ({signum}), pid: {:?}", child.id());
                        let ctrl = match signum {
                            2 => Some(0x03u8),
                            3 => Some(0x1cu8),
                            _ => None,
                        };
                        if let Some(ch) = ctrl {
                            unsafe { libc::write(pty_fd, &ch as *const u8 as *const _, 1) };
                        } else if let Some(pid) = child.id() {
                            unsafe { libc::kill(pid as i32, signum) };
                        }
                    },
                    Some(Signal::Kill) => {
                        log_error(child.start_kill());
                        break;
                    },
                    None => break,
                }
            }
        }

        let exit_code = wait_child(&mut child).await;
        log_error(frames_tx.send(Frame::Exit(exit_code)).await);
        exit_code
}

async fn attach_pipe(mut child: tokio::process::Child, host: HostProcess) -> i32 {
        let child_stdin = child.stdin.take();
        let mut child_stdout = child.stdout.take();
        let mut child_stderr = child.stderr.take();

        let stdin = host.stdin;
        tokio::task::spawn_local(async move {
            if let Some(mut w) = child_stdin {
                if let Err(e) = relay_stdin_pipe(stdin, &mut w).await {
                    error!("stdin failure: {e:#}");
                }
            }
        });

        let (signals_tx, mut signals_rx) = tokio::sync::mpsc::channel(1);
        let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<Frame>(1);
        let _ = host.attachment.send(capnp_rpc::new_client(ProcessImpl {
            frames: RefCell::new(frames_rx),
            signals: RefCell::new(signals_tx),
        }));

        let mut stdout_buf = [0u8; 4096];
        let mut stderr_buf = [0u8; 4096];
        let mut stdout_done = false;
        let mut stderr_done = false;

        loop {
            if stdout_done && stderr_done {
                break;
            }
            tokio::select! {
                res = async { child_stdout.as_mut().unwrap().read(&mut stdout_buf).await },
                    if !stdout_done && child_stdout.is_some() =>
                {
                    match res {
                        Ok(0) | Err(_) => stdout_done = true,
                        Ok(n) => log_error(frames_tx.send(Frame::Stdout(Bytes::copy_from_slice(&stdout_buf[..n]))).await),
                    }
                },
                res = async { child_stderr.as_mut().unwrap().read(&mut stderr_buf).await },
                    if !stderr_done && child_stderr.is_some() =>
                {
                    match res {
                        Ok(0) | Err(_) => stderr_done = true,
                        Ok(n) => log_error(frames_tx.send(Frame::Stderr(Bytes::copy_from_slice(&stderr_buf[..n]))).await),
                    }
                },
                s = signals_rx.recv() => match s {
                    Some(Signal::Num(signum)) => {
                        if let Some(pid) = child.id() {
                            unsafe { libc::kill(pid as i32, signum) };
                        }
                    },
                    Some(Signal::Kill) => {
                        log_error(child.start_kill());
                        break;
                    },
                    None => break,
                }
            }
        }

        let exit_code = wait_child(&mut child).await;
        log_error(frames_tx.send(Frame::Exit(exit_code)).await);
        exit_code
}

async fn wait_child(child: &mut tokio::process::Child) -> i32 {
    match child.wait().await {
        Ok(exit) => exit.code().unwrap_or(-1),
        Err(e) => {
            error!("{e}");
            -1
        }
    }
}

fn log_error<Ok, Err: Display>(res: Result<Ok, Err>) {
    if let Err(e) = res {
        error!("{e}");
    }
}

async fn relay_stdin_pty(stdin: stdin::Client, mut writer: pty_process::OwnedWritePty) -> anyhow::Result<()> {
    loop {
        let response = stdin.read_request().send().promise.await?;
        let input = response.get().and_then(|r| r.get_input())?;
        match input.which()? {
            process_input::Stdin(frame) => {
                match frame?.which() {
                    Ok(data_frame::Data(Ok(data))) => writer.write_all(data).await?,
                    _ => return Ok(()),
                }
            }
            process_input::Resize(size) => {
                let s = size?;
                writer.resize(pty_process::Size::new(s.get_rows(), s.get_cols()))?;
            }
        }
    }
}

async fn relay_stdin_pipe(stdin: stdin::Client, writer: &mut tokio::process::ChildStdin) -> anyhow::Result<()> {
    loop {
        let response = stdin.read_request().send().promise.await?;
        let input = response.get().and_then(|r| r.get_input())?;
        match input.which()? {
            process_input::Stdin(frame) => {
                match frame?.which() {
                    Ok(data_frame::Data(Ok(data))) => writer.write_all(data).await?,
                    _ => return Ok(()),
                }
            }
            process_input::Resize(_) => {} // ignored in pipe mode
        }
    }
}

struct ProcessImpl {
    frames: RefCell<tokio::sync::mpsc::Receiver<Frame>>,
    signals: RefCell<tokio::sync::mpsc::Sender<Signal>>,
}

enum Frame {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(i32),
}

enum Signal {
    Num(i32),
    Kill,
}

impl process::Server for ProcessImpl {
    async fn poll(
        self: Rc<Self>,
        _params: process::PollParams,
        mut results: process::PollResults,
    ) -> Result<(), capnp::Error> {
        let mut next = results.get().init_next();
        match self.frames.borrow_mut().recv().await {
            Some(Frame::Stdout(data)) => next.init_stdout().set_data(&data),
            Some(Frame::Stderr(data)) => next.init_stderr().set_data(&data),
            Some(Frame::Exit(code)) => next.set_exit(code),
            None => return Err(capnp::Error::failed("supervisor process already exited".into())),
        }
        Ok(())
    }

    async fn signal(
        self: Rc<Self>,
        params: process::SignalParams,
        _results: process::SignalResults,
    ) -> Result<(), capnp::Error> {
        let signum = params.get()?.get_signum();
        let _ = self.signals.borrow_mut().send(Signal::Num(signum)).await;
        Ok(())
    }

    async fn kill(
        self: Rc<Self>,
        _params: process::KillParams,
        _results: process::KillResults,
    ) -> Result<(), capnp::Error> {
        let _ = self.signals.borrow_mut().send(Signal::Kill).await;
        Ok(())
    }
}
