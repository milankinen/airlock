//! `airlock exec` — attach a process to a running VM container.
//!
//! Walks up from the current working directory looking for
//! `.airlock/sandbox/cli.sock` and connects there. The command,
//! the caller's CWD, and any `-e KEY=VAL` overrides are forwarded
//! to the `airlock start` process, which already holds the
//! resolved sandbox environment (image env + `airlock.toml` env).
//! The server merges overrides into that base and asks the
//! supervisor to spawn the process. `exec` therefore never loads
//! the project, the vault, or the settings itself.

use std::path::PathBuf;

use airlock_common::cli_capnp::*;
use clap::Args;
use futures::AsyncReadExt;

use crate::runtime::{self, RawTerminalRuntime};
use crate::{oci, rpc, sandbox};

/// CLI arguments for `airlock exec`.
#[derive(Args, Debug)]
pub struct ExecArgs {
    /// Command to run
    pub cmd: String,
    /// Arguments for the command
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// Working directory inside the container
    #[arg(short = 'w', long)]
    pub cwd: Option<String>,
    /// Environment variables (KEY=VALUE)
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,
    /// Run the command inside a login shell (sources /etc/profile, ~/.profile)
    #[arg(short = 'l', long)]
    pub login: bool,
}

/// Entry point for `airlock exec <cmd> [args...]`.
pub async fn main(args: ExecArgs) -> anyhow::Result<i32> {
    let ExecArgs {
        cmd,
        args,
        cwd,
        env,
        login,
    } = args;
    let argv: Vec<String> = std::iter::once(cmd).chain(args).collect();
    let argv = if login {
        oci::apply_login_shell(argv)
    } else {
        argv
    };
    let (cmd, args) = argv.split_first().expect("argv holds the command");

    let host_cwd = std::env::current_dir().map_err(|e| anyhow::anyhow!("get cwd: {e}"))?;
    let sock_path = find_cli_sock(&host_cwd).ok_or_else(|| {
        anyhow::anyhow!(
            "no running sandbox — looked for .airlock/sandbox/cli.sock from {} upward. \
             is 'airlock start' running in this project?",
            host_cwd.display()
        )
    })?;

    let stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .map_err(|e| {
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) {
                anyhow::anyhow!(
                    "stale cli.sock at {} — is 'airlock start' still running?",
                    sock_path.display()
                )
            } else {
                anyhow::anyhow!("failed to connect to {}: {e}", sock_path.display())
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

    let raw = RawTerminalRuntime::new();
    let stdin = raw.stdin()?;
    let pty_size = stdin.pty_size();

    let mut req = cli_service.exec_request();
    req.get().set_stdin(capnp_rpc::new_client(stdin));
    rpc::set_pty(req.get().init_pty(), pty_size);
    req.get().set_cmd(cmd);
    let mut args_b = req.get().init_args(args.len() as u32);
    for (i, a) in args.iter().enumerate() {
        args_b.set(i as u32, a.as_str());
    }
    let cwd = cwd.unwrap_or_else(|| host_cwd.to_string_lossy().into_owned());
    req.get().set_cwd(&cwd);

    let mut env_b = req.get().init_env(env.len() as u32);
    for (i, e) in env.iter().enumerate() {
        env_b.set(i as u32, e.as_str());
    }

    let response = req.send().promise.await?;
    let proc = rpc::Process::new(response.get()?.get_proc()?);

    let mut terminal = raw.into_terminal();
    tokio::task::spawn_local(runtime::forward_signals(runtime::signals()?, proc.clone()));
    Ok(sandbox::io::drive(&proc, &mut terminal).await)
}

/// Walk up from `start` looking for `.airlock/sandbox/`. For each
/// match resolve the CLI sock path — which is either that directory's
/// `cli.sock` or a hash-keyed fallback under `~/.cache/airlock/sock/`
/// when the in-sandbox path would exceed the `AF_UNIX` limit. Returns
/// the first resolved path that exists on disk.
fn find_cli_sock(start: &std::path::Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let sandbox_dir = dir.join(".airlock").join("sandbox");
        if !sandbox_dir.is_dir() {
            continue;
        }
        if let Ok(candidate) = crate::cache::cli_sock_path(&sandbox_dir)
            && candidate.exists()
        {
            return Some(candidate);
        }
    }
    None
}
