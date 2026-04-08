//! `ez exec` — attach a process to a running VM container.
//!
//! Connects to the `cli.sock` Unix socket exposed by a running `ez go` session
//! and sends a `CliService.exec()` RPC to spawn a new process inside the
//! container. I/O is bridged between the host terminal and the guest process.

use std::io::Write;

use ezpez_protocol::supervisor_capnp::*;
use futures::AsyncReadExt;
use tokio::task::LocalSet;

use crate::{cli, project, rpc, terminal};

/// Entry point for `ez exec <cmd> [args...]`.
pub async fn run(cmd: String, args: Vec<String>, cwd: Option<String>, env: Vec<String>) -> i32 {
    let local = LocalSet::new();
    local
        .run_until(async {
            run_inner(cmd, args, cwd, env).await.unwrap_or_else(|e| {
                cli::error!("{e:#}");
                1
            })
        })
        .await
}

async fn run_inner(
    cmd: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Vec<String>,
) -> anyhow::Result<i32> {
    let project = project::load(None)?;
    let sock_path = project.cache_dir.join(ezpez_protocol::CLI_SOCK_FILENAME);

    let stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .map_err(|e| {
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) {
                anyhow::anyhow!("no running VM — is 'ez go' running in this project?")
            } else {
                anyhow::anyhow!("failed to connect to VM: {e}")
            }
        })?;

    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();
    let network = capnp_rpc::twoparty::VatNetwork::new(
        reader,
        writer,
        capnp_rpc::rpc_twoparty_capnp::Side::Client,
        capnp::message::ReaderOptions::default(),
    );
    let mut rpc_sys = capnp_rpc::RpcSystem::new(Box::new(network), None);
    let cli_service: cli_service::Client =
        rpc_sys.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);
    tokio::task::spawn_local(rpc_sys);

    let mut terminal = terminal::setup();
    let stdin = terminal.stdin()?;
    let pty_size = stdin.pty_size();

    // Build exec request
    let mut req = cli_service.exec_request();
    req.get().set_stdin(capnp_rpc::new_client(stdin));
    if let Some((rows, cols)) = pty_size {
        let mut size = req.get().init_pty().init_size();
        size.set_rows(rows);
        size.set_cols(cols);
    } else {
        req.get().init_pty().set_none(());
    }
    req.get().set_cmd(&cmd);
    let mut args_b = req.get().init_args(args.len() as u32);
    for (i, a) in args.iter().enumerate() {
        args_b.set(i as u32, a.as_str());
    }
    let cwd = cwd.unwrap_or_else(|| project.guest_cwd.to_string_lossy().into_owned());
    req.get().set_cwd(&cwd);
    let mut env_b = req.get().init_env(env.len() as u32);
    for (i, e) in env.iter().enumerate() {
        env_b.set(i as u32, e.as_str());
    }

    let response = req.send().promise.await?;
    let proc = rpc::Process::new(response.get()?.get_proc()?);

    // Enter raw mode after the RPC handshake completes
    terminal.enter_raw_mode();

    // Forward host signals to the container process
    let signal_proc = proc.clone();
    let mut signals = terminal::signals()?;
    tokio::task::spawn_local(async move {
        use futures::StreamExt;
        while let Some(signum) = signals.next().await {
            tracing::debug!("forwarding signal {signum} to exec process");
            if let Err(e) = signal_proc.signal(signum).await {
                tracing::error!("signal forward failed: {e}");
            }
        }
    });

    // Output relay
    let exit_code = loop {
        match proc.poll().await {
            Ok(rpc::ProcessEvent::Exit(code)) => break code,
            Ok(rpc::ProcessEvent::Stdout(data)) => {
                tracing::trace!(
                    "exec stdout: {} bytes: {:?}",
                    data.len(),
                    String::from_utf8_lossy(&data)
                );
                let _ = std::io::stdout().write_all(&data);
                let _ = std::io::stdout().flush();
            }
            Ok(rpc::ProcessEvent::Stderr(data)) => {
                tracing::trace!("exec stderr: {} bytes", data.len());
                let _ = std::io::stderr().write_all(&data);
                let _ = std::io::stderr().flush();
            }
            Err(e) => {
                tracing::trace!("exec poll error: {e}");
                break 1;
            }
        }
    };

    Ok(exit_code)
}
