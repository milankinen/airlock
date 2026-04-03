mod assets;
mod cli;
mod config;
mod error;
mod oci;
mod project;
mod rpc;
mod terminal;
mod vm;

use std::io::Write;
use crate::error::CliError;
use crate::rpc::process::ProcessEvent;
use clap::Parser;
use tokio::task::LocalSet;
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() {
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
    let host_ports = config.network.host_ports.clone();
    let project = project::ensure(config)?;
    let bundle = oci::prepare(&project).await?;

    eprintln!("Booting VM...");
    let (_vm, vsock_fd) = vm::start(&project, bundle).await?;

    let supervisor = rpc::Supervisor::connect(vsock_fd)?;
    eprintln!("supervisor connected");

    let terminal = project.config.terminal;
    let pty_size = if terminal {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        Some((rows, cols))
    } else {
        None
    };

    let _guard = if terminal { Some(terminal::TerminalGuard::enter()) } else { None };
    let resizes = if terminal { Some(terminal::resizes()?) } else { None };
    let stdin_cap =
        capnp_rpc::new_client(rpc::stdin::StdinImpl::new(resizes));

    let proc = supervisor
        .start(stdin_cap, pty_size, &project.ca_cert, &project.ca_key, host_ports)
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

    loop {
        match proc.poll().await {
            Ok(ProcessEvent::Exit(code)) => return Ok(code),
            Ok(ProcessEvent::Stdout(data)) => {
                let _ = std::io::stdout().write_all(&data);
                let _ = std::io::stdout().flush();
            }
            Ok(ProcessEvent::Stderr(data)) => {
                let _ = std::io::stderr().write_all(&data);
                let _ = std::io::stderr().flush();
            }
            Err(_) => return Ok(1),
        }
    }
}
