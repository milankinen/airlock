//! The `airlock exec` command.
//!
//! Runs a process in the container of a running sandbox. The `airlock start`
//! process of that sandbox does the work.

use std::path::PathBuf;

use airlock_common::cli_capnp::*;
use clap::Args;
use futures::AsyncReadExt;

use crate::context::Context;
use crate::runtime::{self, RawTerminalRuntime};
use crate::{oci, rpc, sandbox, sandboxes};

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
    /// Run the command in a login shell (reads /etc/profile, ~/.profile)
    #[arg(short = 'l', long)]
    pub login: bool,
}

/// Entry point for `airlock exec <cmd> [args...]`.
/// Returns:
///   Exit code of the process, or error if no sandbox runs.
// The command, the caller's cwd and the `-e KEY=VAL` overrides go to the
// `airlock start` process. That process has the resolved sandbox environment
// (image env and `airlock.toml` env). It merges the overrides into that env
// and tells the supervisor to start the process. Thus `exec` never loads the
// project or the vault. It uses only the sandbox registry of the context.
pub async fn main(args: ExecArgs, context: &Context) -> anyhow::Result<i32> {
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
    // The registry has canonical project paths.
    let lookup_dir = std::fs::canonicalize(&host_cwd).unwrap_or_else(|_| host_cwd.clone());
    let sock_path = find_cli_sock(context, &lookup_dir).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "no running sandbox for {} or its parent directories. \
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

/// Find the CLI socket of the nearest running sandbox at or above `start`.
/// Sandboxes in the data directory and in the project both count.
/// Returns:
///   The first socket path that exists, or `None`.
// A sandbox without a socket does not run. The search then goes on to the
// parent directories. The socket path is `cli.sock` in the sandbox
// directory. If that path is longer than the `AF_UNIX` limit, the path is a
// hash-keyed fallback under `<data>/sock/`.
pub(super) async fn find_cli_sock(
    context: &Context,
    start: &std::path::Path,
) -> anyhow::Result<Option<PathBuf>> {
    for found in sandboxes::candidates(context, start, true).await? {
        if !found.dir.is_dir() {
            continue;
        }
        if let Ok(candidate) = crate::cache::cli_sock_path(&context.data_dir, &found.dir)
            && candidate.exists()
        {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}
