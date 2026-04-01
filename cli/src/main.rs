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
use config::Config;
use tokio::task::LocalSet;
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = cli::Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(if cli.verbose { "debug" } else { "warn" }))
        .with_writer(std::io::stderr)
        .init();

    let config = Config {
        cpus: cli.cpus,
        memory_mb: cli.memory,
        verbose: cli.verbose,
        ..Config::default()
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

async fn run(config: Config) -> Result<i32, CliError> {
    let project_hash = project::project_hash()?;
    let resolved = oci::resolve(&config.image).await?;
    let image_dir = oci::ensure_image(&resolved).await?;
    let bundle_path = oci::ensure_project(
        &image_dir,
        &resolved.image_config,
        &resolved.digest,
        &project_hash,
    )?;

    // Boot VM with the prepared bundle
    eprintln!("Booting VM...");
    let (_vm, vsock_fd) = vm::create(&config, &bundle_path).await?;

    let client = rpc::Client::connect(vsock_fd)?;
    eprintln!("supervisor connected");

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let shell = client.exec(tokio::io::stdin(), rows, cols).await?;
    let shell_ref = &shell;

    let _guard = terminal::TerminalGuard::enter();
    let mut resizes = terminal::resizes()?;

    let resize_loop = async move {
        while resizes.recv().await.is_some() {
            if let Ok((cols, rows)) = crossterm::terminal::size() {
                let _ = shell_ref.resize(rows, cols).await;
            }
        }
    };
    let std_loop = async {
        loop {
            match shell_ref.poll().await? {
                ProcessEvent::Exit(code) => return Ok::<i32, anyhow::Error>(code),
                ProcessEvent::Stdout(data) => {
                    let _ = std::io::stdout().write_all(&data);
                    let _ = std::io::stdout().flush();
                }
                ProcessEvent::Stderr(data) => {
                    let _ = std::io::stderr().write_all(&data);
                    let _ = std::io::stderr().flush();
                }
            }
        }
    };

    Ok(tokio::select! {
        code = std_loop => code,
        _ = resize_loop => Ok(-1)
    }?)
}
