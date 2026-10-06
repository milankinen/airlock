//! Cap'n Proto RPC server for the supervisor.
//!
//! Implements the `Supervisor` interface: the host CLI calls `boot()` once
//! to bring up the VM (mounts, networking, daemons) and may then call
//! `spawn()`, any number of times, to run processes inside it (the main
//! shell and `airlock exec` alike). `spawn()` is refused until `boot()`
//! has succeeded. The `shutdown()` call syncs filesystems before the VM is
//! destroyed.

use std::cell::RefCell;
use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::Arc;

use airlock_common::network_capnp::network_proxy;
use airlock_common::supervisor_capnp::*;
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty};
use futures::AsyncReadExt;

use crate::admin::DenyTracker;
use crate::daemon::{self, DaemonSet, DaemonSpec};
use crate::init::{
    CacheConfig, DirMountConfig, FileMountConfig, InitConfig, MaskConfig, MountConfig,
};
use crate::net;
use crate::process::spawn_user;
use crate::stats::Collector;

/// Unix socket forwarding pair: host-side path and guest-side path.
#[allow(dead_code)]
pub struct SocketForwardConfig {
    pub host: String,
    pub guest: String,
}

/// All configuration received in the `Supervisor.boot()` RPC call. Passed
/// to the `boot` closure which bootstraps the container. Carries no
/// process to run — the main shell and `airlock exec` both start
/// afterwards via `spawn()`.
pub struct BootConfig {
    pub log_sink: log_sink::Client,
    pub log_filter: String,
    pub network: network_proxy::Client,
    pub sockets: Vec<SocketForwardConfig>,
    pub uid: u32,
    pub gid: u32,
    pub nested_virt: bool,
    pub init_config: InitConfig,
    pub mount_config: MountConfig,
    /// Clipboard grant. A withheld capability disables the bridge outright.
    pub clipboard: crate::clipboard::ClipboardConfig,
    /// Browser grant. A withheld capability disables the bridge outright.
    pub browser: crate::browser::BrowserConfig,
    /// Sidecar daemons to start after init. The callback decides when to
    /// call `DaemonSet::start_all` (typically right after `init::setup`).
    pub daemons: Vec<DaemonSpec>,
    /// Shared slot the boot callback writes the constructed `DaemonSet`
    /// into so `pollDaemons`/`shutdownDaemons` can reach it later.
    pub daemon_set_slot: Rc<RefCell<Option<DaemonSet>>>,
}

/// Host-side handles for a single process's I/O.
pub struct HostProcess {
    pub stdin: stdin::Client,
    /// Oneshot to deliver the `Process` capability back to the host once the
    /// child is spawned. Taken by the startup code — `None` after consumption.
    pub result: Option<tokio::sync::oneshot::Sender<Result<process::Client, String>>>,
}

/// Accept the RPC connection and run the boot callback. Returns once the
/// `boot` call has been answered (success or failure) — the caller keeps
/// the VM alive afterwards; there is no process here to wait on.
pub async fn serve<Boot: AsyncFn(BootConfig) -> anyhow::Result<()>>(
    conn_fd: OwnedFd,
    deny_tracker: Arc<DenyTracker>,
    network: network_proxy::Client,
    boot: Boot,
) -> anyhow::Result<()> {
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(conn_fd.into_raw_fd()) };
    std_stream.set_nonblocking(true)?;
    let stream = tokio::net::TcpStream::from_std(std_stream)?;
    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();

    let transport = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Server,
        capnp::message::ReaderOptions::default(),
    );

    let (boot_tx, boot_rx) = tokio::sync::oneshot::channel::<BootPayload>();

    let daemon_set: Rc<RefCell<Option<DaemonSet>>> = Rc::new(RefCell::new(None));
    let client: supervisor::Client = capnp_rpc::new_client(SupervisorImpl {
        boot_tx: RefCell::new(Some(boot_tx)),
        spawn_creds: RefCell::new(None),
        stats: RefCell::new(Collector::new()),
        deny_tracker,
        daemon_set,
        network: network.clone(),
    });
    let rpc = RpcSystem::new(Box::new(transport), Some(client.client));

    tokio::task::spawn_local(rpc);
    let (cfg, result_tx) = boot_rx.await.expect("host connection failed");
    match boot(cfg).await {
        Ok(()) => {
            let _ = result_tx.send(Ok(()));
        }
        Err(e) => {
            tracing::error!("supervisor boot error: {e:#}");
            let _ = result_tx.send(Err(format!("{e:#}")));
        }
    }
    Ok(())
}

/// What `boot()`'s RPC handler hands to [`serve`]: the parsed config, and a
/// oneshot to report whether the boot closure succeeded.
type BootPayload = (BootConfig, tokio::sync::oneshot::Sender<Result<(), String>>);

/// Server-side implementation of the `Supervisor` Cap'n Proto interface.
///
/// `boot_tx` is consumed by the first `boot()` call and set to `None` —
/// a second call is refused because the VM only supports one boot sequence.
/// `spawn_creds` is set once `boot()` succeeds and read by `spawn()` to run
/// container processes with the same uid/gid/hardening; `spawn()` before
/// that is refused.
struct SupervisorImpl {
    boot_tx: RefCell<Option<tokio::sync::oneshot::Sender<BootPayload>>>,
    spawn_creds: RefCell<Option<(u32, u32, bool)>>,
    stats: RefCell<Collector>,
    deny_tracker: Arc<DenyTracker>,
    /// Populated by the boot callback once daemons have been started. Held
    /// by `Rc` so the same handle lives in `BootConfig.daemon_set_slot`.
    daemon_set: Rc<RefCell<Option<DaemonSet>>>,
    /// Bootstrap capability of the separate network vsock connection,
    /// threaded into `BootConfig` so downstream net modules can reach
    /// the host without going through this supervisor channel.
    network: network_proxy::Client,
}

impl supervisor::Server for SupervisorImpl {
    async fn boot(
        self: Rc<Self>,
        params: supervisor::BootParams,
        _results: supervisor::BootResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;

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
                    mount_key: f.get_key()?.to_str()?.to_string(),
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

        let daemons = daemon::parse_specs(params.get_daemons()?)?;

        let masks = params
            .get_masks()?
            .iter()
            .map(|m| {
                let paths = m
                    .get_paths()?
                    .iter()
                    .map(|p| Ok(p?.to_str()?.to_string()))
                    .collect::<Result<Vec<_>, capnp::Error>>()?;
                Ok(MaskConfig {
                    name: m.get_name()?.to_str()?.to_string(),
                    paths,
                })
            })
            .collect::<Result<Vec<_>, capnp::Error>>()?;

        // A host that grants nothing leaves this default-initialised: both
        // flags false and a null `sink`, which is exactly "no clipboard".
        let cb = params.get_clipboard()?;
        let clipboard = crate::clipboard::ClipboardConfig {
            copy: cb.get_copy(),
            paste: cb.get_paste(),
            sink: if cb.has_sink() {
                Some(cb.get_sink()?)
            } else {
                None
            },
            limit: cb.get_limit(),
        };

        // Likewise a null `sink` is exactly "no browser".
        let br = params.get_browser()?;
        let browser = crate::browser::BrowserConfig {
            sink: if br.has_sink() {
                Some(br.get_sink()?)
            } else {
                None
            },
        };

        let cfg = BootConfig {
            clipboard,
            browser,
            log_sink: params.get_logs()?,
            log_filter: params.get_log_filter()?.to_str()?.to_string(),
            network: self.network.clone(),
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
            uid,
            gid,
            nested_virt: params.get_nested_virt(),
            init_config: InitConfig {
                epoch: params.get_epoch(),
                epoch_nanos: params.get_epoch_nanos(),
                host_ports: params.get_host_ports()?.iter().collect(),
            },
            mount_config: MountConfig {
                image_id: params.get_image_id()?.to_str()?.to_string(),
                image_layers: params
                    .get_image_layers()?
                    .iter()
                    .map(|s| s.map(|t| t.to_str().unwrap_or("").to_string()))
                    .collect::<Result<Vec<_>, _>>()?,
                dirs,
                files,
                caches,
                masks,
                ca_cert: params.get_ca_cert()?.to_vec(),
            },
            daemons,
            daemon_set_slot: Rc::clone(&self.daemon_set),
        };

        let Some(tx) = self.boot_tx.borrow_mut().take() else {
            return Err(capnp::Error::failed("boot already called".into()));
        };
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let _ = tx.send((cfg, result_tx));

        match result_rx.await {
            Ok(Ok(())) => {
                // Credentials are recorded only now: a failed boot must
                // leave spawn() refused.
                *self.spawn_creds.borrow_mut() = Some((uid, gid, harden));
                Ok(())
            }
            Ok(Err(msg)) => Err(capnp::Error::failed(msg)),
            Err(_) => Err(capnp::Error::failed("supervisor boot dropped".into())),
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

    async fn spawn(
        self: Rc<Self>,
        params: supervisor::SpawnParams,
        mut results: supervisor::SpawnResults,
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

        let (uid, gid, harden) = (*self.spawn_creds.borrow())
            .ok_or_else(|| capnp::Error::failed("spawn called before boot succeeded".into()))?;

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
            Err(_) => Err(capnp::Error::failed("spawn setup dropped".into())),
        }
    }

    async fn report_deny(
        self: Rc<Self>,
        params: supervisor::ReportDenyParams,
        _results: supervisor::ReportDenyResults,
    ) -> Result<(), capnp::Error> {
        let epoch = params.get()?.get_epoch();
        self.deny_tracker.record(epoch);
        Ok(())
    }

    async fn open_local_tcp(
        self: Rc<Self>,
        params: supervisor::OpenLocalTcpParams,
        mut results: supervisor::OpenLocalTcpResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let port = params.get_port();
        let client = params.get_client()?;

        let server = net::open_local_tcp(port, client)
            .await
            .map_err(|e| capnp::Error::failed(format!("open 127.0.0.1:{port}: {e}")))?;
        results.get().set_server(server);
        Ok(())
    }

    async fn poll_stats(
        self: Rc<Self>,
        _params: supervisor::PollStatsParams,
        mut results: supervisor::PollStatsResults,
    ) -> Result<(), capnp::Error> {
        let snapshot = self.stats.borrow_mut().poll();

        let mut out = results.get().init_snapshot();

        {
            let mut cpu = out.reborrow().init_cpu();
            let mut pc = cpu.reborrow().init_per_core(snapshot.per_core.len() as u32);
            for (i, v) in snapshot.per_core.iter().enumerate() {
                pc.set(i as u32, *v);
            }
        }
        {
            let mut mem = out.reborrow().init_memory();
            mem.set_total_bytes(snapshot.total_bytes);
            mem.set_used_bytes(snapshot.used_bytes);
        }
        {
            let (one, five, fifteen) = snapshot.load_avg;
            let mut la = out.reborrow().init_load_average();
            la.set_one(one);
            la.set_five(five);
            la.set_fifteen(fifteen);
        }

        Ok(())
    }

    async fn poll_daemons(
        self: Rc<Self>,
        _params: supervisor::PollDaemonsParams,
        mut results: supervisor::PollDaemonsResults,
    ) -> Result<(), capnp::Error> {
        let snapshot = match self.daemon_set.borrow().as_ref() {
            Some(ds) => ds.snapshot(),
            None => Vec::new(),
        };
        let list = results.get().init_states(snapshot.len() as u32);
        daemon::write_status_list(&snapshot, list);
        Ok(())
    }

    async fn shutdown_daemons(
        self: Rc<Self>,
        _params: supervisor::ShutdownDaemonsParams,
        _results: supervisor::ShutdownDaemonsResults,
    ) -> Result<(), capnp::Error> {
        if let Some(ds) = self.daemon_set.borrow().as_ref() {
            ds.shutdown_all();
        }
        Ok(())
    }

    async fn sync_clock(
        self: Rc<Self>,
        params: supervisor::SyncClockParams,
        _results: supervisor::SyncClockResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        crate::init::set_clock(params.get_epoch(), params.get_epoch_nanos());
        Ok(())
    }
}
