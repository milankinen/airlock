//! `airlock start` — boot the VM and run the container.
//!
//! Orchestrates the full lifecycle: load config → pull OCI image → set up
//! network rules → boot VM → start supervisor RPC → relay I/O → clean up.

use std::io::Write;

use clap::Args;
use dialoguer::Select;
use dialoguer::theme::ColorfulTheme;
use tokio::task::LocalSet;
use tracing_subscriber::EnvFilter;

use crate::cli::{self, CliArgs, LogLevel};
use crate::runtime::{MonitorRuntime, RawTerminalRuntime, Runtime, Terminal};
use crate::vault::Vault;
use crate::{cli_server, config, network, oci, project, rpc, runtime, vm};

/// Default `airlock.toml` written when initializing a new sandbox.
const DEFAULT_CONFIG: &str = "[vm]\n# image = \"alpine:latest\"\n";

/// CLI arguments for `airlock start`.
#[derive(Args, Debug)]
pub struct StartArgs {
    /// Log level
    #[arg(long, env = "AIRLOCK_LOG_LEVEL", default_value = "info")]
    pub log_level: LogLevel,
    /// Working directory inside the container (defaults to the host cwd)
    #[arg(long)]
    pub sandbox_cwd: Option<String>,
    /// Run the container command inside a login shell (sources /etc/profile, ~/.profile)
    #[arg(short = 'l', long)]
    pub login: bool,
    /// Show detailed output (mounts, network rules)
    #[arg(short = 'v', long)]
    pub verbose: bool,
    /// Open TUI monitoring control panel (tabbed sandbox + network view)
    #[arg(short = 'm', long)]
    pub monitor: bool,
}

/// Entry point for `airlock start [--log-level <level>] [-- extra-args...]`.
pub async fn main(args: StartArgs, extra_args: Vec<String>, vault: Vault) -> i32 {
    let local = LocalSet::new();
    cli::set_verbose(args.verbose);

    #[cfg(target_os = "linux")]
    vm::require_kvm();

    let host_cwd = match std::env::current_dir() {
        Ok(p) => std::fs::canonicalize(&p).unwrap_or(p),
        Err(e) => {
            cli::error!("Cannot determine current directory: {e}");
            return 1;
        }
    };

    // Step 1: Create .airlock/ directory and initialize logging early
    // so that config loading and preset resolution are observable.
    let cache_dir = match project::ensure_cache_dir(&host_cwd) {
        Ok(d) => d,
        Err(e) => {
            cli::error!("Failed to create .airlock directory: {e}");
            return 1;
        }
    };
    setup_logging(args.log_level, &cache_dir);

    // Step 2: Load config and start the VM
    let has_config = ["toml", "json", "yaml", "yml"].iter().any(|ext| {
        host_cwd.join(format!("airlock.{ext}")).exists()
            || host_cwd.join(format!("airlock.local.{ext}")).exists()
    });
    if !has_config {
        if !cli::is_interactive() {
            cli::error!("No airlock.toml found in {}", host_cwd.display());
            return 2;
        }
        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(format!("No airlock.toml found in {}", host_cwd.display()))
            .items(["Initialize with defaults", "Cancel"])
            .default(0)
            .interact()
            .unwrap_or(1);
        if selection != 0 {
            cli::error!("Aborted.");
            return 0;
        }
        if let Err(e) = std::fs::write(host_cwd.join("airlock.toml"), DEFAULT_CONFIG) {
            cli::error!("Failed to create airlock.toml: {e}");
            return 1;
        }
        cli::log!("Created airlock.toml in {}", host_cwd.display());
    }

    let config = match config::load(&host_cwd) {
        Ok(c) => c,
        Err(e) => {
            cli::error!("Config error: {e:#}");
            return 2;
        }
    };

    let cli_args = CliArgs::new(args.log_level, extra_args, args.login);
    let sandbox_cwd = args.sandbox_cwd;
    local
        .run_until(async {
            let result = if args.monitor {
                run(
                    cli_args,
                    config,
                    host_cwd,
                    sandbox_cwd,
                    vault,
                    MonitorRuntime::new(),
                )
                .await
            } else {
                run(
                    cli_args,
                    config,
                    host_cwd,
                    sandbox_cwd,
                    vault,
                    RawTerminalRuntime::new(),
                )
                .await
            };
            result.unwrap_or_else(|e| {
                cli::error!("Error: {e:?}");
                1
            })
        })
        .await
}

async fn run(
    args: CliArgs,
    config: config::Config,
    host_cwd: std::path::PathBuf,
    project_cwd: Option<String>,
    vault: Vault,
    mut runtime: impl Runtime,
) -> anyhow::Result<i32> {
    let project = project::lock(host_cwd, config, project_cwd, vault)?;
    print_preparing(&project);

    let image = oci::prepare(&project).await?;
    let network = network::setup(&project, &image.container_home)?;

    print_mounts_and_rules(&project);

    // Check if user interrupted during setup (e.g. Ctrl+C during download)
    if cli::is_interrupted() {
        return Ok(130); // 128 + SIGINT
    }

    cli::log!("Booting VM...");
    let (vm, vsock_fd) = vm::start(&args, &project, &image).await?;
    project.save_meta();

    let supervisor = rpc::Supervisor::connect(vsock_fd)?;
    network.deny_reporter().attach(supervisor.client());

    let (stdin_client, pty_size) = runtime.attach_stdin()?;
    let signals = runtime.signals()?;

    // Launch the output sink (enters raw mode for the raw runtime, spawns the
    // TUI thread for the monitor runtime) before `supervisor.start` consumes
    // `network`.
    let mut terminal = runtime.launch(&project, &network, supervisor.clone())?;
    // When AIRLOCK_PTY_DUMP=1, write all guest PTY output to
    // <sandbox_dir>/pty.dump for offline replay/diagnosis.
    let mut pty_dump = pty_dump_file(&project.sandbox_dir);

    // Connected to airlockd - finalize vm init start main proc
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let epoch = now.as_secs();
    let epoch_nanos = now.subsec_nanos();
    let proc = supervisor
        .start(
            &args,
            &project,
            &vm,
            stdin_client,
            pty_size,
            network,
            epoch,
            epoch_nanos,
        )
        .await?;

    // Start CLI server so `airlock exec` can attach processes to this VM.
    // The server needs a copy of the sandbox's resolved env so it can layer
    // `airlock exec -e KEY=VAL` overrides on top without the exec client
    // having to re-resolve the project.
    let sock_path = project.sandbox_dir.join(airlock_common::CLI_SOCK_FILENAME);
    let base_env = vm.env.clone();
    tokio::task::spawn_local(cli_server::serve(sock_path, supervisor.clone(), base_env));

    spawn_signal_forwarder(signals, proc.clone());
    let exit_code = poll_proc(&proc, &mut terminal, pty_dump.as_mut()).await;
    let final_code = terminal.exit(exit_code);

    // Sync filesystems before killing VM
    supervisor.shutdown().await;

    // Drain file-sync events then destroy VM.
    vm.shutdown().await;
    Ok(final_code)
}

/// Forward host signals (SIGHUP/SIGINT/SIGQUIT/SIGTERM/SIGUSR1/SIGUSR2) to the
/// guest process on a background task.
fn spawn_signal_forwarder(mut signals: runtime::SignalStream, proc: rpc::Process) {
    tokio::task::spawn_local(async move {
        use futures::StreamExt;
        while let Some(signum) = signals.next().await {
            tracing::debug!("forwarding signal {signum} to VM");
            if let Err(e) = proc.signal(signum).await {
                tracing::error!("signal forward failed: {e}");
            }
        }
    });
}

/// Drive the guest process to completion: relay stdout/stderr into `terminal`
/// (and optional PTY dump) until an Exit event or RPC error is observed.
async fn poll_proc(
    proc: &rpc::Process,
    terminal: &mut impl Terminal,
    mut pty_dump: Option<&mut std::fs::File>,
) -> i32 {
    loop {
        match proc.poll().await {
            Ok(rpc::ProcessEvent::Exit(code)) => return code,
            Ok(rpc::ProcessEvent::Stdout(data)) => {
                tracing::trace!(
                    "host stdout: {} bytes: {:?}",
                    data.len(),
                    String::from_utf8_lossy(&data)
                );
                write_pty_dump(pty_dump.as_deref_mut(), &data);
                terminal.stdout(&data);
            }
            Ok(rpc::ProcessEvent::Stderr(data)) => {
                tracing::trace!("host stderr: {} bytes", data.len());
                write_pty_dump(pty_dump.as_deref_mut(), &data);
                terminal.stderr(&data);
            }
            Err(e) => {
                tracing::trace!("host poll error: {e}");
                return 1;
            }
        }
    }
}

/// Print the "Preparing sandbox" header with image name and CA cert status.
fn print_preparing(project: &project::Project) {
    cli::log!("Preparing sandbox...");
    cli::log!(
        "  {} config loaded, image: {}",
        cli::check(),
        cli::dim(&project.config.vm.image.name)
    );
    if project.ca_newly_generated {
        cli::log!("  {} ca cert generated", cli::check());
    }
}

/// Verbose-only: list enabled mounts and network rules grouped by kind.
fn print_mounts_and_rules(project: &project::Project) {
    let enabled_mounts: Vec<_> = project
        .config
        .mounts
        .iter()
        .filter(|(_, m)| m.enabled)
        .collect();
    if !enabled_mounts.is_empty() {
        cli::verbose!("  {} mounts: {}", cli::bullet(), enabled_mounts.len());
        for (key, mount) in &enabled_mounts {
            cli::verbose!("      {key}: {} \u{2192} {}", mount.source, mount.target);
        }
    }
    let enabled_rules: Vec<_> = project
        .config
        .network
        .rules
        .iter()
        .filter(|(_, r)| r.enabled)
        .collect();
    if !enabled_rules.is_empty() {
        let policy = format!("{:?}", project.config.network.policy).to_lowercase();
        cli::verbose!(
            "  {} network rules: {} (policy: {policy})",
            cli::bullet(),
            enabled_rules.len()
        );
        for (key, rule) in &enabled_rules {
            cli::verbose!(
                "      {key}: allow {} deny {}",
                rule.allow.len(),
                rule.deny.len()
            );
        }
    }
}

/// Open the PTY dump file if `AIRLOCK_PTY_DUMP=1`, otherwise `None`.
fn pty_dump_file(sandbox_dir: &std::path::Path) -> Option<std::fs::File> {
    if std::env::var("AIRLOCK_PTY_DUMP").as_deref() != Ok("1") {
        return None;
    }
    let path = sandbox_dir.join("pty.dump");
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        Ok(f) => {
            cli::log!("PTY dump: {}", path.display());
            Some(f)
        }
        Err(e) => {
            cli::error!("Failed to open PTY dump {}: {e}", path.display());
            None
        }
    }
}

fn write_pty_dump(file: Option<&mut std::fs::File>, data: &[u8]) {
    if let Some(f) = file {
        let _ = f.write_all(data);
    }
}

fn setup_logging(log_level: LogLevel, cache_dir: &std::path::Path) {
    let filter = CliArgs::log_filter_for(log_level);
    if let Ok(log_file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(cache_dir.join("airlock.log"))
    {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(filter))
            .with_writer(std::sync::Mutex::new(log_file))
            .with_ansi(false)
            .init();
    }
}
