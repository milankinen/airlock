mod assets;
mod cli;
mod error;
mod terminal;
mod vm;

use clap::Parser;
use cli::Cli;
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> error::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(if cli.verbose {
            "debug"
        } else {
            "warn"
        }))
        .with_writer(std::io::stderr)
        .init();

    // Extract embedded kernel + initramfs (or use CLI overrides)
    let asset_paths = match (&cli.kernel, &cli.initramfs) {
        (Some(k), Some(i)) => assets::AssetPaths {
            kernel: k.clone(),
            initramfs: i.clone(),
            _tmp: tempfile::tempdir()?,
        },
        _ => assets::extract_assets()?,
    };

    let vm_config = vm::config::VmConfig {
        cpus: cli.cpus,
        memory_bytes: cli.memory * 1024 * 1024,
        kernel: asset_paths.kernel,
        initramfs: asset_paths.initramfs,
        kernel_cmdline: if cli.verbose {
            "console=hvc0 rdinit=/init".to_string()
        } else {
            "console=hvc0 rdinit=/init quiet loglevel=3".to_string()
        },
    };

    #[cfg(target_os = "macos")]
    {
        use vm::VmBackend;

        eprintln!("Booting VM...");
        let mut backend = vm::apple::AppleVmBackend::new(vm_config)?;
        backend.start().await?;

        let (write_fd, read_fd) = backend.console_fds();

        tokio::select! {
            r = terminal::run_relay(write_fd, read_fd) => { r?; }
            _ = backend.wait_for_stop() => {}
        }

        drop(backend);
        std::process::exit(0);
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = vm_config;
        Err(error::Error::VmRuntime(
            "only macOS is supported currently".into(),
        ))
    }
}
