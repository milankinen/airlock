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
                            return Err(error::Error::VmRuntime(format!(
                                "supervisor not reachable after {attempts} attempts: {e}"
                            )));
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        };

        // Establish RPC connection
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

        // Get terminal size
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

        // Channel to receive exit code from shell
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();

        // Open shell via RPC — supervisor creates PTY
        let shell_output_client: ezpez_protocol::supervisor_capnp::output_stream::Client =
            capnp_rpc::new_client(ShellOutputImpl { exit_tx: std::cell::RefCell::new(Some(exit_tx)) });

        let mut req = supervisor_client.open_shell_request();
        req.get().set_rows(rows);
        req.get().set_cols(cols);
        req.get().set_stdout(shell_output_client);

        let response = req
            .send()
            .promise
            .await
            .map_err(|e| error::Error::VmRuntime(format!("openShell failed: {e}")))?;
        let shell_input = response
            .get()
            .map_err(|e| error::Error::VmRuntime(format!("{e}")))?
            .get_stdin()
            .map_err(|e| error::Error::VmRuntime(format!("{e}")))?;

        eprintln!("supervisor connected");

        // Enter raw mode and relay stdin to shell via RPC
        let _guard = terminal::TerminalGuard::enter();

        let stdin_relay = async {
            let mut stdin = tokio::io::stdin();
            let mut buf = [0u8; 1024];
            loop {
                use tokio::io::AsyncReadExt;
                let n = stdin.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                let mut req = shell_input.write_request();
                req.get().set_data(&buf[..n]);
                if req.send().await.is_err() {
                    break; // Shell exited
                }
            }
            Ok::<(), error::Error>(())
        };

        let exit_code;
        tokio::select! {
            _ = stdin_relay => { exit_code = 0; }
            code = exit_rx => { exit_code = code.unwrap_or(1); }
            _ = backend.wait_for_stop() => { exit_code = 1; }
        }

        drop(_guard);
        drop(backend);
        std::process::exit(exit_code);
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = vm_config;
        Err(error::Error::VmRuntime(
            "only macOS is supported currently".into(),
        ))
    }
}

/// Receives shell output from supervisor via RPC callback
struct ShellOutputImpl {
    exit_tx: std::cell::RefCell<Option<tokio::sync::oneshot::Sender<i32>>>,
}

impl ezpez_protocol::supervisor_capnp::output_stream::Server for ShellOutputImpl {
    async fn write(
        self: std::rc::Rc<Self>,
        params: ezpez_protocol::supervisor_capnp::output_stream::WriteParams,
    ) -> Result<(), capnp::Error> {
        let data = params.get()?.get_data()?;
        use std::io::Write;
        let _ = std::io::stdout().write_all(data);
        let _ = std::io::stdout().flush();
        Ok(())
    }

    async fn done(
        self: std::rc::Rc<Self>,
        params: ezpez_protocol::supervisor_capnp::output_stream::DoneParams,
        _results: ezpez_protocol::supervisor_capnp::output_stream::DoneResults,
    ) -> Result<(), capnp::Error> {
        let exit_code = params.get()?.get_exit_code();
        if let Some(tx) = self.exit_tx.borrow_mut().take() {
            let _ = tx.send(exit_code);
        }
        Ok(())
    }
}
