mod assets;
mod cache;
mod cli;
mod config;

pub(crate) mod network;
mod oci;
mod project;
mod rpc;
mod terminal;
mod vm;

use std::io::Write;

use tokio::task::LocalSet;
use tracing_subscriber::EnvFilter;

use crate::cli::CliArgs;
use crate::project::Project;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    #[cfg(target_os = "linux")]
    vm::check_kvm_access();
    let args = cli::initialize();
    let cwd = std::env::current_dir().unwrap_or_default();
    let config = match config::load(&cwd) {
        Ok(c) => c,
        Err(e) => {
            cli::error!("config error: {e:#}");
            std::process::exit(2);
        }
    };
    let local = LocalSet::new();
    let exit_code = local
        .run_until(async {
            run(args, config).await.unwrap_or_else(|e| {
                cli::error!("error: {e:?}");
                1
            })
        })
        .await;

    std::process::exit(exit_code);
}

async fn run(args: CliArgs, config: config::Config) -> anyhow::Result<i32> {
    let project = project::lock(config)?;
    setup_logging(&args, &project);

    let mut terminal = terminal::setup();
    let bundle = oci::prepare(&args, &project, &terminal).await?;
    let network = network::setup(&project)?;

    // Check if user interrupted during setup (e.g. Ctrl+C during download)
    if cli::is_interrupted() {
        return Ok(130); // 128 + SIGINT
    }

    // Enter raw mode only after downloads complete so Ctrl+C works during setup
    terminal.enter_raw_mode();

    cli::log!("Booting VM...");
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (vm_handle, vsock_fd) = vm::start(&args, &project, bundle).await?;
    let supervisor = rpc::Supervisor::connect(vsock_fd)?;

    let stdin = terminal.stdin()?;
    let proc = supervisor
        .start(&args, &project, stdin, network, epoch)
        .await?;

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

    // Destroy VM and return the exit code from vm shell
    drop(vm_handle);
    Ok(exit_code)
}

fn setup_logging(args: &CliArgs, project: &Project) {
    if let Ok(log_file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(project.cache_dir.join("ez.log"))
    {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(args.log_filter()))
            .with_writer(std::sync::Mutex::new(log_file))
            .with_ansi(false)
            .init();
    }
}
