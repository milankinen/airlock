//! `airlock exec` — attach a process to a running VM container.
//!
//! Connects to the `cli.sock` Unix socket exposed by a running `airlock start` session
//! and sends a `CliService.exec()` RPC to spawn a new process inside the
//! container. I/O is bridged between the host terminal and the guest process.

use std::io::Write;

use airlock_protocol::supervisor_capnp::*;
use clap::Args;
use futures::AsyncReadExt;
use tokio::task::LocalSet;

use crate::runtime::{self, RawTerminalRuntime};
use crate::vault::Vault;
use crate::{cli, project, rpc};

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
pub async fn main(args: ExecArgs, vault: Vault) -> i32 {
    let local = LocalSet::new();
    local
        .run_until(async {
            run(args, vault).await.unwrap_or_else(|e| {
                cli::error!("{e:#}");
                1
            })
        })
        .await
}

async fn run(args: ExecArgs, vault: Vault) -> anyhow::Result<i32> {
    let ExecArgs {
        cmd,
        args,
        cwd,
        env,
        login,
    } = args;
    let project = project::load(vault)?;
    let (cmd, args) = if login {
        apply_login_shell(cmd, args)
    } else {
        (cmd, args)
    };
    let sock_path = project
        .sandbox_dir
        .join(airlock_protocol::CLI_SOCK_FILENAME);

    let stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .map_err(|e| {
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) {
                anyhow::anyhow!("no running VM — is 'airlock up' running in this project?")
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

    let mut terminal = RawTerminalRuntime::new();
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

    // Merge config env vars (from airlock.toml / airlock.local.toml) with CLI-passed ones.
    // CLI `-e` flags take precedence over config values.
    let resolved_env = resolve_config_env(&project, &env)?;
    let mut env_b = req.get().init_env(resolved_env.len() as u32);
    for (i, e) in resolved_env.iter().enumerate() {
        env_b.set(i as u32, e.as_str());
    }

    let response = req.send().promise.await?;
    let proc = rpc::Process::new(response.get()?.get_proc()?);

    // Enter raw mode after the RPC handshake completes
    terminal.enter_raw_mode();

    // Forward host signals to the container process
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

/// Resolve config env vars (with `${VAR}` substitution) and merge with CLI-passed env.
/// CLI `-e KEY=VALUE` entries override config values for the same key.
fn resolve_config_env(
    project: &project::Project,
    cli_env: &[String],
) -> anyhow::Result<Vec<String>> {
    let mut merged: Vec<String> = Vec::new();

    // Collect keys from CLI env so we can skip config entries that are overridden.
    let cli_keys: std::collections::HashSet<&str> = cli_env
        .iter()
        .filter_map(|e| e.split_once('=').map(|(k, _)| k))
        .collect();

    for (key, template) in &project.config.env {
        if cli_keys.contains(key.as_str()) {
            continue;
        }
        let value = project
            .vault
            .subst(template)
            .map_err(|e| anyhow::anyhow!("env.{key}: {e}"))?;
        merged.push(format!("{key}={value}"));
    }

    merged.extend_from_slice(cli_env);
    Ok(merged)
}

fn is_shell_name(cmd: &str) -> bool {
    let name = std::path::Path::new(cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(cmd);
    matches!(name, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh") || name.ends_with("sh")
}
