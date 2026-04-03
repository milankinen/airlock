mod assets;
mod cli;
mod config;
mod error;
mod network;
mod oci;
mod project;
mod rpc;
mod terminal;
mod vm;

use std::io::Write;
use crate::error::CliError;
use clap::Parser;
use tokio::task::LocalSet;
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cli = cli::Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(if cli.verbose { "debug" } else { "warn" }))
        .with_writer(std::io::stderr)
        .init();

    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let config = config::Config {
        cpus: cli.cpus,
        memory_mb: cli.memory,
        verbose: cli.verbose,
        args: cli.args,
        terminal: is_tty,
        ..config::Config::default()
    };

    let local = LocalSet::new();
    let exit_code = local.run_until(async {
        match run(config).await {
            Ok(code) => code,
            Err(CliError::Expected(msg)) => {
                eprintln!("error: {msg}");
                1
            }
            Err(CliError::Unexpected(e)) => {
                eprintln!("{e:#}");
                2
            }
        }
    }).await;

    std::process::exit(exit_code);
}

async fn run(config: config::Config) -> Result<i32, CliError> {
    let project = project::ensure(config)?;
    let vm = vm::prepare(&project)?;
    let bundle = oci::prepare(&project, &vm).await?;
    let network = network::setup(&project)?;
    let terminal = terminal::setup(&project)?;

    eprintln!("Booting VM...");
    let (vm_handle, vsock_fd) = vm.start(&project, bundle).await?;
    let supervisor = rpc::Supervisor::connect(vsock_fd)?;
    eprintln!("supervisor connected");

    let stdin = terminal.stdin()?;
    let proc = supervisor
        .start(&project, stdin, network)
        .await?;

    // Forward host signals to the VM process
    let signal_proc = proc.clone();
    let mut signals = terminal::signals()?;
    tokio::task::spawn_local(async move {
        use futures::StreamExt;
        while let Some(signum) = signals.next().await {
            tracing::debug!("forwarding signal {signum} to VM");
            if let Err(e) = signal_proc.signal(signum).await {
                tracing::debug!("signal forward failed: {e}");
            }
        }
    });

    // Handle VM shell output and exit code delegation
    let exit_code = loop {
        match proc.poll().await {
            Ok(rpc::ProcessEvent::Exit(code)) => break code,
            Ok(rpc::ProcessEvent::Stdout(data)) => {
                let _ = std::io::stdout().write_all(&data);
                let _ = std::io::stdout().flush();
            }
            Ok(rpc::ProcessEvent::Stderr(data)) => {
                let _ = std::io::stderr().write_all(&data);
                let _ = std::io::stderr().flush();
            }
            Err(_) => break 1,
        }
    };

    // Destroy VM and return the exit code from vm shell
    drop(vm_handle);
    Ok(exit_code)
}
