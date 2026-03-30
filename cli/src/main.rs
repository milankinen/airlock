mod assets;
mod cli;
mod error;
mod terminal;
mod vm;

use clap::Parser;
use cli::Cli;
use tracing_subscriber::EnvFilter;

fn main() -> error::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(if cli.verbose {
            "debug"
        } else {
            "warn"
        }))
        .with_writer(std::io::stderr)
        .init();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();

    local.block_on(&rt, async_main(cli))
}

async fn async_main(cli: Cli) -> error::Result<()> {
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

        // Connect to supervisor via vsock (retry until ready)
        let vsock_fd = {
            let mut attempts = 0;
            loop {
                match backend.vsock_connect(ezpez_protocol::SUPERVISOR_PORT).await {
                    Ok(fd) => break fd,
                    Err(e) => {
                        attempts += 1;
                        if attempts >= 30 {
                            return Err(error::Error::VmRuntime(
                                format!("supervisor not reachable after {attempts} attempts: {e}"),
                            ));
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        };

        // Establish RPC connection to supervisor
        let supervisor_client = {
            use futures::AsyncReadExt;
            use std::os::unix::io::{FromRawFd, IntoRawFd};

            let std_stream = unsafe { std::net::TcpStream::from_raw_fd(vsock_fd.into_raw_fd()) };
            std_stream.set_nonblocking(true)?;
            let stream = tokio::net::TcpStream::from_std(std_stream)?;
            let (reader, writer) =
                tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();

            let network = capnp_rpc::twoparty::VatNetwork::new(
                reader,
                writer,
                capnp_rpc::rpc_twoparty_capnp::Side::Client,
                Default::default(),
            );

            let mut rpc = capnp_rpc::RpcSystem::new(Box::new(network), None);
            let client: ezpez_protocol::supervisor_capnp::supervisor::Client =
                rpc.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);

            tokio::task::spawn_local(rpc);
            client
        };

        // Ping/pong test
        {
            let request = supervisor_client.ping_request();
            let response = request.send().promise.await
                .map_err(|e| error::Error::VmRuntime(format!("ping failed: {e}")))?;
            let id = response.get()
                .map_err(|e| error::Error::VmRuntime(format!("{e}")))?
                .get_id();
            eprintln!("supervisor connected (pong id={id})");
        }

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
