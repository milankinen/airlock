//! `airlock up` — boot the VM and run the container.
//!
//! Orchestrates the full lifecycle: load config → pull OCI image → set up
//! network rules → boot VM → start supervisor RPC → relay I/O → clean up.

use std::io::Write;
use std::path::PathBuf;

use dialoguer::Select;
use dialoguer::theme::ColorfulTheme;
use tokio::task::LocalSet;
use tracing_subscriber::EnvFilter;

use crate::cli::{self, CliArgs, LogLevel};
use crate::project::{self, Project};
use crate::{cli_server, config, network, oci, rpc, terminal, vm};

/// Default `airlock.toml` written when initializing a new project.
const DEFAULT_CONFIG: &str = "[vm]\n# image = \"alpine:latest\"\n";

/// Entry point for `airlock up [path] [--log-level <level>] [-- extra-args...]`.
pub async fn run(
    path: Option<String>,
    log_level: LogLevel,
    extra_args: Vec<String>,
    project_cwd: Option<String>,
    login: bool,
) -> i32 {
    if !cli::is_interactive() {
        cli::error!("airlock up requires a TTY");
        return 2;
    }

    #[cfg(target_os = "linux")]
    vm::require_kvm();

    // Resolve project directory
    let host_cwd = match resolve_project_dir(path) {
        Ok(p) => p,
        Err(e) => {
            cli::error!("{e:#}");
            return 1;
        }
    };

    // Ensure a config file exists; offer to initialize if not
    let has_config = ["toml", "json", "yaml", "yml"].iter().any(|ext| {
        host_cwd.join(format!("airlock.{ext}")).exists()
            || host_cwd.join(format!("airlock.local.{ext}")).exists()
    });
    if !has_config {
        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(format!("No airlock.toml found in {}", host_cwd.display()))
            .items(["Initialize with defaults", "Cancel"])
            .default(0)
            .interact()
            .unwrap_or(1);
        if selection != 0 {
            println!("Aborted.");
            return 0;
        }
        if let Err(e) = std::fs::write(host_cwd.join("airlock.toml"), DEFAULT_CONFIG) {
            cli::error!("failed to create airlock.toml: {e}");
            return 1;
        }
        cli::log!("Created airlock.toml in {}", host_cwd.display());
    }

    let config = match config::load(&host_cwd) {
        Ok(c) => c,
        Err(e) => {
            cli::error!("config error: {e:#}");
            return 2;
        }
    };

    let args = CliArgs::new(log_level, extra_args, login);
    let local = LocalSet::new();
    local
        .run_until(async {
            run_inner(args, config, host_cwd, project_cwd)
                .await
                .unwrap_or_else(|e| {
                    cli::error!("error: {e:?}");
                    1
                })
        })
        .await
}

fn resolve_project_dir(path: Option<String>) -> anyhow::Result<PathBuf> {
    let p = if let Some(s) = path {
        let p = PathBuf::from(s);
        if !p.is_dir() {
            anyhow::bail!("not a directory: {}", p.display());
        }
        std::fs::canonicalize(&p).unwrap_or(p)
    } else {
        let cwd = std::env::current_dir()?;
        std::fs::canonicalize(&cwd).unwrap_or(cwd)
    };
    Ok(p)
}

async fn run_inner(
    args: CliArgs,
    config: config::Config,
    host_cwd: PathBuf,
    project_cwd: Option<String>,
) -> anyhow::Result<i32> {
    let project = project::lock(host_cwd, config, project_cwd)?;
    setup_logging(&args, &project);

    let short_id = project.id()[..7].to_string();
    cli::log!("Preparing project {short_id}...");
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
    let network = network::setup(&project, &image.container_home)?;

    // Check if user interrupted during setup (e.g. Ctrl+C during download)
    if cli::is_interrupted() {
        return Ok(130); // 128 + SIGINT
    }

    // Enter raw mode only after downloads complete so Ctrl+C works during setup
    terminal.enter_raw_mode();

    cli::log!("Booting VM...");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let epoch = now.as_secs();
    let epoch_nanos = now.subsec_nanos();
    let (vm, vsock_fd) = vm::start(&args, &project, &image).await?;
    project.save_meta();
    let supervisor = rpc::Supervisor::connect(vsock_fd)?;

    let stdin = terminal.stdin()?;
    let proc = supervisor
        .start(&args, &project, &vm, stdin, network, epoch, epoch_nanos)
        .await?;

    // Start CLI server so `airlock exec` can attach processes to this VM
    let sock_path = project.cache_dir.join(airlock_protocol::CLI_SOCK_FILENAME);
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

    // Handle VM shell output and exit code delegation
    let exit_code = loop {
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
    };

    // Sync filesystems before killing VM
    supervisor.shutdown().await;

    // Drain file-sync events then destroy VM.
    vm.shutdown().await;
    Ok(exit_code)
}

fn setup_logging(args: &CliArgs, project: &Project) {
    if let Ok(log_file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(project.cache_dir.join("airlock.log"))
    {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(args.log_filter()))
            .with_writer(std::sync::Mutex::new(log_file))
            .with_ansi(false)
            .init();
    }
}
