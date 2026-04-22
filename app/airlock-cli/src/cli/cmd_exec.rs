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

use std::io::Write;
use std::path::PathBuf;

use airlock_common::cli_capnp::*;
use clap::Args;
use futures::AsyncReadExt;

use crate::rpc;
use crate::runtime::{self, RawTerminalRuntime};

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
    let (cmd, args) = if login {
        apply_login_shell(cmd, args)
    } else {
        (cmd, args)
    };

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

    let mut terminal = RawTerminalRuntime::new();
    let stdin = terminal.stdin()?;
    let pty_size = stdin.pty_size();

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
    let cwd = cwd.unwrap_or_else(|| host_cwd.to_string_lossy().into_owned());
    req.get().set_cwd(&cwd);

    let mut env_b = req.get().init_env(env.len() as u32);
    for (i, e) in env.iter().enumerate() {
        env_b.set(i as u32, e.as_str());
    }

    let response = req.send().promise.await?;
    let proc = rpc::Process::new(response.get()?.get_proc()?);

    terminal.enter_raw_mode();

    let signal_proc = proc.clone();
    let mut signals = runtime::signals()?;
    tokio::task::spawn_local(async move {
        use futures::StreamExt;
        while let Some(signum) = signals.next().await {
            tracing::debug!("forwarding signal {signum} to exec process");
            if let Err(e) = signal_proc.signal(signum).await {
                tracing::error!("signal forward failed: {e}");
            }
        }
    });

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

/// Wrap `(cmd, args)` for execution inside a login shell.
///
/// If `cmd` is a lone shell binary (no args), adds `-l` directly.
/// Otherwise wraps as `sh -l -c 'exec "$0" "$@"' cmd args...` — the
/// `$0`/`$@` trick passes args without any quoting.
fn apply_login_shell(cmd: String, args: Vec<String>) -> (String, Vec<String>) {
    let is_lone_shell = args.is_empty() && is_shell_name(&cmd);
    if is_lone_shell {
        (cmd, vec!["-l".to_string()])
    } else {
        let mut new_args = vec![
            "-l".to_string(),
            "-c".to_string(),
            r#"exec "$0" "$@""#.to_string(),
            cmd,
        ];
        new_args.extend(args);
        ("bash".to_string(), new_args)
    }
}

fn is_shell_name(cmd: &str) -> bool {
    let name = std::path::Path::new(cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(cmd);
    matches!(name, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh") || name.ends_with("sh")
}
