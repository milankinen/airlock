mod assets;
mod cli;
mod error;
mod terminal;
mod vm;

use clap::Parser;
use cli::Cli;
use ezpez_protocol::supervisor_capnp::*;
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
        bundle_path: Some(".tmp/bundle".into()),
    };

    #[cfg(target_os = "macos")]
    {
        use vm::VmBackend;

        eprintln!("Booting VM...");
        let mut backend = vm::apple::AppleVmBackend::new(vm_config)?;
        backend.start().await?;

        // Connect to supervisor via vsock
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
            let client: supervisor::Client =
                rpc.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);

            tokio::task::spawn_local(rpc);
            client
        };

        // Exec shell — pass stdin stream + PTY config
        let stdin_client: byte_stream::Client = capnp_rpc::new_client(StdinStream);
        let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());

        // Always allocate PTY for shell (needed for echo/job control).
        // Terminal size from host, or default 80x24 if not a TTY.
        let (cols, rows) = if is_tty {
            crossterm::terminal::size().unwrap_or((80, 24))
        } else {
            (80, 24)
        };

        let mut req = supervisor_client.exec_request();
        req.get().set_stdin(stdin_client);
        let mut size = req.get().init_pty().init_size();
        size.set_rows(rows);
        size.set_cols(cols);

        let response = req
            .send()
            .promise
            .await
            .map_err(|e| error::Error::VmRuntime(format!("exec failed: {e}")))?;
        let proc = response
            .get()
            .map_err(|e| error::Error::VmRuntime(format!("{e}")))?
            .get_proc()
            .map_err(|e| error::Error::VmRuntime(format!("{e}")))?;

        eprintln!("supervisor connected");

        // Enter raw mode if on a TTY
        let _guard = if is_tty {
            Some(terminal::TerminalGuard::enter())
        } else {
            None
        };

        // Watch for terminal resize (SIGWINCH)
        let proc_for_resize = proc.clone();
        if is_tty {
            tokio::task::spawn_local(async move {
                let mut sigwinch = match tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::window_change(),
                ) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                while sigwinch.recv().await.is_some() {
                    if let Ok((cols, rows)) = crossterm::terminal::size() {
                        let mut req = proc_for_resize.resize_request();
                        let mut size = req.get().init_size();
                        size.set_rows(rows);
                        size.set_cols(cols);
                        let _ = req.send().promise.await;
                    }
                }
            });
        }

        // Poll loop: read process output until exit
        let exit_code = loop {
            let response = proc
                .poll_request()
                .send()
                .promise
                .await
                .map_err(|e| error::Error::VmRuntime(format!("poll failed: {e}")))?;
            let next = response
                .get()
                .map_err(|e| error::Error::VmRuntime(format!("{e}")))?
                .get_next()
                .map_err(|e| error::Error::VmRuntime(format!("{e}")))?;

            match next.which() {
                Ok(process_output::Exit(code)) => break code,
                Ok(process_output::Stdout(frame)) => {
                    let frame = frame.map_err(|e| error::Error::VmRuntime(format!("{e}")))?;
                    if let Ok(data_frame::Data(Ok(data))) = frame.which() {
                        use std::io::Write;
                        let _ = std::io::stdout().write_all(data);
                        let _ = std::io::stdout().flush();
                    }
                }
                Ok(process_output::Stderr(frame)) => {
                    let frame = frame.map_err(|e| error::Error::VmRuntime(format!("{e}")))?;
                    if let Ok(data_frame::Data(Ok(data))) = frame.which() {
                        use std::io::Write;
                        let _ = std::io::stderr().write_all(data);
                        let _ = std::io::stderr().flush();
                    }
                }
                Err(_) => {
                    break 1;
                }
            }
        };

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

/// Stdin ByteStream — supervisor calls read() to pull input from us
struct StdinStream;

impl byte_stream::Server for StdinStream {
    async fn read(
        self: std::rc::Rc<Self>,
        _params: byte_stream::ReadParams,
        mut results: byte_stream::ReadResults,
    ) -> Result<(), capnp::Error> {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 1024];
        let mut stdin = tokio::io::stdin();
        match stdin.read(&mut buf).await {
            Ok(0) => {
                results.get().init_frame().set_eof(());
            }
            Ok(n) => {
                results.get().init_frame().set_data(&buf[..n]);
            }
            Err(e) => {
                results
                    .get()
                    .init_frame()
                    .set_err(&format!("{e}"));
            }
        }
        Ok(())
    }
}
