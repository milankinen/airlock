//! Cap'n Proto RPC server for the supervisor.
//!
//! Implements the `Supervisor` interface: the host CLI calls `start()` once to
//! bootstrap the VM and launch the main process, and may later call `exec()`
//! to attach sidecar processes. The `shutdown()` call syncs filesystems before
//! the VM is destroyed.

use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};
use std::rc::Rc;

use airlock_protocol::supervisor_capnp::*;
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty};
use futures::AsyncReadExt;

use crate::init::{CacheConfig, DirMountConfig, FileMountConfig, InitConfig, MountConfig};
use crate::process::{SpawnedProcess, spawn_root, spawn_user};

/// Unix socket forwarding pair: host-side path and guest-side path.
pub struct SocketForwardConfig {
    pub host: String,
    pub guest: String,
}

/// All configuration received in the `Supervisor.start()` RPC call.
/// Passed to the `init` closure which bootstraps the container.
pub struct StartConfig {
    pub log_sink: log_sink::Client,
    pub log_filter: String,
    pub network: network_proxy::Client,
    pub sockets: Vec<SocketForwardConfig>,
    pub cmd: String,
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub uid: u32,
    pub gid: u32,
    pub nested_virt: bool,
    pub harden: bool,
    pub init_config: InitConfig,
    pub mount_config: MountConfig,
    pub pty_size: Option<(u16, u16)>,
}

/// Host-side handles for a single process's I/O.
pub struct HostProcess {
    pub stdin: stdin::Client,
    /// Oneshot to deliver the `Process` capability back to the host once the
    /// child is spawned. Taken by the startup code — `None` after consumption.
    pub result: Option<tokio::sync::oneshot::Sender<Result<process::Client, String>>>,
}

/// Accept the RPC connection, run the init callback, and block until the
/// main process exits. Returns the process exit code.
pub async fn start<Init: AsyncFn(StartConfig) -> anyhow::Result<SpawnedProcess>>(
    conn_fd: OwnedFd,
    init: Init,
) -> anyhow::Result<i32> {
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(conn_fd.into_raw_fd()) };
    std_stream.set_nonblocking(true)?;
    let stream = tokio::net::TcpStream::from_std(std_stream)?;
    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();

    let network = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Server,
        capnp::message::ReaderOptions::default(),
    );

    let (conn_tx, conn_rx) = tokio::sync::oneshot::channel::<ConnPayload>();

    let client: supervisor::Client = capnp_rpc::new_client(SupervisorImpl {
        start_tx: std::cell::RefCell::new(Some(conn_tx)),
        exec_creds: std::cell::RefCell::new(None),
    });
    let rpc = RpcSystem::new(Box::new(network), Some(client.client));

    tokio::task::spawn_local(rpc);
    let (cfg, host_proc) = conn_rx.await.expect("host connection failed");
    let proc = match init(cfg).await {
        Ok(proc) => proc,
        Err(e) => {
            tracing::error!("supervisor init error: {e:#}");
            // Escape single quotes so the message can be embedded in a
            // single-quoted shell string: ' → '\''
            // Use {e} (not {e:#}) to avoid duplicating the chain when
            // the outer error and its source have the same display string.
            let msg = format!("{e}").replace('\'', r"'\''");
            let script = format!("printf '%s\\n' 'error: {msg}' >&2; exit 100");
            spawn_root("/bin/sh", &["-c", &script], None)?
        }
    };
    Ok(proc.attach(host_proc).await)
}

type ConnPayload = (StartConfig, HostProcess);

/// Server-side implementation of the `Supervisor` Cap'n Proto interface.
///
/// `start_tx` is consumed by the first `start()` call and set to `None` —
/// subsequent calls are rejected because the VM only supports one init sequence.
/// `uid_gid` is set during `start()` and read by `exec()` to reuse container credentials.
struct SupervisorImpl {
    start_tx: std::cell::RefCell<Option<tokio::sync::oneshot::Sender<ConnPayload>>>,
    exec_creds: std::cell::RefCell<Option<(u32, u32, bool)>>,
}

impl supervisor::Server for SupervisorImpl {
    async fn start(
        self: Rc<Self>,
        params: supervisor::StartParams,
        mut results: supervisor::StartResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;

        let pty_size = match params.get_pty()?.which() {
            Ok(pty_config::Size(size)) => {
                let size = size?;
                Some((size.get_rows(), size.get_cols()))
            }
            _ => None,
        };

        let uid = params.get_uid();
        let gid = params.get_gid();
        let harden = params.get_harden();

        let dirs = params
            .get_dirs()?
            .iter()
            .map(|d| {
                Ok(DirMountConfig {
                    tag: d.get_tag()?.to_str()?.to_string(),
                    target: d.get_target()?.to_str()?.to_string(),
                    read_only: d.get_read_only(),
                })
            })
            .collect::<Result<Vec<_>, capnp::Error>>()?;

        let files = params
            .get_files()?
            .iter()
            .map(|f| {
                Ok(FileMountConfig {
                    target: f.get_target()?.to_str()?.to_string(),
                    read_only: f.get_read_only(),
                })
            })
            .collect::<Result<Vec<_>, capnp::Error>>()?;

        let caches = params
            .get_caches()?
            .iter()
            .map(|c| {
                let paths = c
                    .get_paths()?
                    .iter()
                    .map(|p| Ok(p?.to_str()?.to_string()))
                    .collect::<Result<Vec<_>, capnp::Error>>()?;
                Ok(CacheConfig {
                    name: c.get_name()?.to_str()?.to_string(),
                    enabled: c.get_enabled(),
                    paths,
                })
            })
            .collect::<Result<Vec<_>, capnp::Error>>()?;

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();

        let cfg = StartConfig {
            log_sink: params.get_logs()?,
            log_filter: params.get_log_filter()?.to_str()?.to_string(),
            network: params.get_network()?,
            sockets: params
                .get_sockets()?
                .iter()
                .map(|s| {
                    Ok(SocketForwardConfig {
                        host: s.get_host()?.to_str()?.to_string(),
                        guest: s.get_guest()?.to_str()?.to_string(),
                    })
                })
                .collect::<Result<Vec<_>, capnp::Error>>()?,
            cmd: params.get_cmd()?.to_str()?.to_string(),
            args: params
                .get_args()?
                .iter()
                .map(|a| a.map(|s| s.to_str().unwrap_or("").to_string()))
                .collect::<Result<Vec<_>, _>>()?,
            env: params
                .get_env()?
                .iter()
                .map(|e| e.map(|s| s.to_str().unwrap_or("").to_string()))
                .collect::<Result<Vec<_>, _>>()?,
            cwd: params.get_cwd()?.to_str()?.to_string(),
            uid,
            gid,
            nested_virt: params.get_nested_virt(),
            harden,
            init_config: InitConfig {
                epoch: params.get_epoch(),
                host_ports: params.get_host_ports()?.iter().collect(),
            },
            mount_config: MountConfig {
                image_id: params.get_image_id()?.to_str()?.to_string(),
                dirs,
                files,
                caches,
            },
            pty_size,
        };

        let host_proc = HostProcess {
            stdin: params.get_stdin()?,
            result: Some(result_tx),
        };

        // Store credentials so exec() can reuse them for container processes.
        *self.exec_creds.borrow_mut() = Some((uid, gid, harden));

        if let Some(tx) = self.start_tx.borrow_mut().take() {
            let _ = tx.send((cfg, host_proc));
        }

        match result_rx.await {
            Ok(Ok(proc)) => {
                results.get().set_proc(proc);
                Ok(())
            }
            Ok(Err(msg)) => Err(capnp::Error::failed(msg)),
            Err(_) => Err(capnp::Error::failed("supervisor setup dropped".into())),
        }
    }

    async fn shutdown(
        self: Rc<Self>,
        _params: supervisor::ShutdownParams,
        _results: supervisor::ShutdownResults,
    ) -> Result<(), capnp::Error> {
        tracing::info!("shutdown: syncing filesystems");
        unsafe { libc::sync() };
        tracing::info!("shutdown: sync complete");
        Ok(())
    }

    async fn exec(
        self: Rc<Self>,
        params: supervisor::ExecParams,
        mut results: supervisor::ExecResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;

        let cmd = params.get_cmd()?.to_str()?.to_string();
        let args: Vec<String> = params
            .get_args()?
            .iter()
            .map(|a| a.map(|s| s.to_str().unwrap_or("").to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let cwd = params.get_cwd()?.to_str()?.to_string();
        let env: Vec<String> = params
            .get_env()?
            .iter()
            .map(|e| e.map(|s| s.to_str().unwrap_or("").to_string()))
            .collect::<Result<Vec<_>, _>>()?;

        let pty_size = match params.get_pty()?.which() {
            Ok(pty_config::Size(size)) => {
                let size = size?;
                Some((size.get_rows(), size.get_cols()))
            }
            _ => None,
        };

        let (uid, gid, harden) = (*self.exec_creds.borrow()).ok_or_else(|| {
            capnp::Error::failed("exec called before container was started".into())
        })?;

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let host = HostProcess {
            stdin: params.get_stdin()?,
            result: Some(result_tx),
        };

        let proc = spawn_user(&cmd, &args, &env, &cwd, uid, gid, harden, pty_size)
            .map_err(|e| capnp::Error::failed(e.to_string()))?;

        tokio::task::spawn_local(async move {
            proc.attach(host).await;
        });

        match result_rx.await {
            Ok(Ok(proc_client)) => {
                results.get().set_proc(proc_client);
                Ok(())
            }
            Ok(Err(msg)) => Err(capnp::Error::failed(msg)),
            Err(_) => Err(capnp::Error::failed("exec setup dropped".into())),
        }
    }
}
