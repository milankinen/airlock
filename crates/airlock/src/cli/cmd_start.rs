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
use crate::{cli_server, config, network, oci, project, rpc, terminal, vm};

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
pub async fn run(args: StartArgs, extra_args: Vec<String>) -> i32 {
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
    let monitor = args.monitor;
    let local = LocalSet::new();
    local
        .run_until(async {
            run_inner(cli_args, config, host_cwd, sandbox_cwd, monitor)
                .await
                .unwrap_or_else(|e| {
                    cli::error!("Error: {e:?}");
                    1
                })
        })
        .await
}

async fn run_inner(
    args: CliArgs,
    config: config::Config,
    host_cwd: std::path::PathBuf,
    project_cwd: Option<String>,
    monitor: bool,
) -> anyhow::Result<i32> {
    let project = project::lock(host_cwd, config, project_cwd)?;

    cli::log!("Preparing sandbox...");
    cli::log!(
        "  {} config loaded, image: {}",
        cli::check(),
        cli::dim(&project.config.vm.image.name)
    );
    if project.ca_newly_generated {
        cli::log!("  {} ca cert generated", cli::check());
    }

    let mut terminal = terminal::setup();
    let image = oci::prepare(&args, &project, &terminal).await?;

    // Create network event channel for TUI monitor
    let (event_tx, event_rx) = if monitor {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let network = network::setup(&project, &image.container_home, event_tx)?;

    // Show mounts and network rules grouped (verbose)
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

    // Check if user interrupted during setup (e.g. Ctrl+C during download)
    if cli::is_interrupted() {
        return Ok(130); // 128 + SIGINT
    }

    // Enter raw mode only after downloads complete so Ctrl+C works during setup
    terminal.enter_raw_mode();

    let policy_str = format!("{:?}", project.config.network.policy).to_lowercase();

    cli::log!("Booting VM...");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let epoch = now.as_secs();
    let epoch_nanos = now.subsec_nanos();
    let (vm, vsock_fd) = vm::start(&args, &project, &image).await?;
    project.save_meta();

    let supervisor = rpc::Supervisor::connect(vsock_fd)?;

    // In monitor mode, use channel-based stdin; otherwise use real terminal stdin.
    // Both produce a stdin::Client for the supervisor RPC.
    let (stdin_tx, stdin_client, pty_size) = if monitor {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let tui_stdin = airlock_tui::TuiStdin::new(rx, Some((rows, cols)));
        let pty_size = tui_stdin.pty_size();
        (Some(tx), capnp_rpc::new_client(tui_stdin), pty_size)
    } else {
        let stdin = terminal.stdin()?;
        let pty_size = stdin.pty_size();
        (None, capnp_rpc::new_client(stdin), pty_size)
    };

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

    // Start CLI server so `airlock exec` can attach processes to this VM
    let sock_path = project
        .sandbox_dir
        .join(airlock_protocol::CLI_SOCK_FILENAME);
    tokio::task::spawn_local(cli_server::serve(sock_path, supervisor.clone()));

    // Forward host signals to the VM process
    let signal_proc = proc.clone();
    let mut signals = terminal::signals()?;
    tokio::task::spawn_local(async move {
        use futures::StreamExt;
        while let Some(signum) = signals.next().await {
            tracing::debug!("forwarding signal {signum} to VM");
            if let Err(e) = signal_proc.signal(signum).await {
                tracing::error!("signal forward failed: {e}");
            }
        }
    });

    let exit_code = if let (Some(stdin_tx), Some(mut event_rx)) = (stdin_tx, event_rx) {
        // TUI monitor mode: spawn TUI on its own thread, keep process
        // polling on the LocalSet so RPC is never blocked by rendering.
        drop(terminal);
        let tui = airlock_tui::spawn(stdin_tx, policy_str);

        // Forward network events from the tokio channel to the TUI thread
        let net_tx = tui.tx.clone();
        tokio::task::spawn_local(async move {
            while let Some(ev) = event_rx.recv().await {
                net_tx.send_network(ev);
            }
        });

        // Process poll loop — forward output to TUI, signal exit when done
        let tui_tx = tui.tx.clone();
        loop {
            match proc.poll().await {
                Ok(rpc::ProcessEvent::Stdout(data) | rpc::ProcessEvent::Stderr(data)) => {
                    tui_tx.send_output(data);
                }
                Ok(rpc::ProcessEvent::Exit(code)) => {
                    tui_tx.send_exit(code);
                    break;
                }
                Err(_) => {
                    tui_tx.send_exit(1);
                    break;
                }
            }
        }
        tui.join()?
    } else {
        // Standard mode: direct stdout relay
        loop {
            match proc.poll().await {
                Ok(rpc::ProcessEvent::Exit(code)) => break code,
                Ok(rpc::ProcessEvent::Stdout(data)) => {
                    tracing::trace!(
                        "host stdout: {} bytes: {:?}",
                        data.len(),
                        String::from_utf8_lossy(&data)
                    );
                    let _ = std::io::stdout().write_all(&data);
                    let _ = std::io::stdout().flush();
                }
                Ok(rpc::ProcessEvent::Stderr(data)) => {
                    tracing::trace!("host stderr: {} bytes", data.len());
                    let _ = std::io::stderr().write_all(&data);
                    let _ = std::io::stderr().flush();
                }
                Err(e) => {
                    tracing::trace!("host poll error: {e}");
                    break 1;
                }
            }
        }
    };

    // Sync filesystems before killing VM
    supervisor.shutdown().await;

    // Drain file-sync events then destroy VM.
    vm.shutdown().await;
    Ok(exit_code)
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
