//! Tests for `airlock network forward`: the command and the CLI server of
//! `airlock start`, over a real Unix socket and real TCP. Only the guest is
//! fake: its loopback is the host loopback.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use airlock_common::supervisor_capnp::{listen_handle, local_connector, supervisor};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle};

use crate::cli::cmd_network::{Mode, Report, forward};
use crate::cli::sandbox_client::SandboxClient;
use crate::cli_server::{self, ForwardRules};
use crate::config::config_values::{Policy, PortMapping};
use crate::messages;
use crate::network::NetworkControl;
use crate::network::io::{ChannelSink, RelayError, RpcTransport, rpc_pipe};
use crate::network::reverse_forward::relay_stream;
use crate::rpc::Supervisor;
use crate::test_cfg::network::network_from_toml;
use crate::test_cfg::{
    TempDir, block_on_local, echo_server, eventually_async, free_port, resolve_project_toml,
    rpc_loopback, serve_echo, temp_dir,
};

/// Guest supervisor with only the two loopback calls. The guest loopback
/// is the host loopback.
struct LoopbackGuest;

impl supervisor::Server for LoopbackGuest {
    async fn connect_to_guest_port(
        self: Rc<Self>,
        params: supervisor::ConnectToGuestPortParams,
        mut results: supervisor::ConnectToGuestPortResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let stream = TcpStream::connect(("127.0.0.1", params.get_port()))
            .await
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        let (host, sink) = rpc_pipe(params.get_client()?);
        tokio::task::spawn_local(relay_stream(stream, host));
        results.get().set_server(sink);
        Ok(())
    }

    async fn bind_host_port(
        self: Rc<Self>,
        params: supervisor::BindHostPortParams,
        mut results: supervisor::BindHostPortResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let port = params.get_port();
        let connector = params.get_connector()?;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| capnp::Error::failed(format!("bind 127.0.0.1:{port}: {e}")))?;
        let task = tokio::task::spawn_local(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::task::spawn_local(guest_connection(stream, connector.clone()));
            }
        });
        results
            .get()
            .set_handle(capnp_rpc::new_client(GuestListener(task.abort_handle())));
        Ok(())
    }
}

/// Send one accepted guest connection to the host connector. A refused
/// connect closes the stream.
async fn guest_connection(stream: TcpStream, connector: local_connector::Client) {
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    let error: RelayError = Rc::new(RefCell::new(None));
    let mut req = connector.connect_request();
    req.get()
        .set_client(capnp_rpc::new_client(ChannelSink::new(tx, error)));
    let Ok(response) = req.send().promise.await else {
        return;
    };
    let Ok(sink) = response
        .get()
        .and_then(local_connector::connect_results::Reader::get_server)
    else {
        return;
    };
    relay_stream(stream, RpcTransport::new(Bytes::new(), rx, sink)).await;
}

/// Lifetime token of a fake guest listener. Its drop stops the listener.
struct GuestListener(AbortHandle);

impl listen_handle::Server for GuestListener {}

impl Drop for GuestListener {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// CLI server of a fake running sandbox.
struct Sandbox {
    _dir: TempDir,
    sock: PathBuf,
    server: JoinHandle<()>,
    network: NetworkControl,
}

impl Sandbox {
    /// Serve the CLI socket for a sandbox with the config `toml`.
    async fn start(toml: &str) -> Self {
        Self::start_with(toml, false).await
    }

    /// Same as [`Self::start`]. With `public_only`, the sandbox has the
    /// network of the install boot.
    async fn start_with(toml: &str, public_only: bool) -> Self {
        let config = resolve_project_toml(toml).unwrap().values;
        let mut network = network_from_toml(toml).unwrap().network;
        if public_only {
            network = network.public_only();
        }
        let network = network.handle().control();
        let rules = ForwardRules::new(&config.network, network.clone());
        let supervisor: supervisor::Client =
            rpc_loopback(capnp_rpc::new_client::<supervisor::Client, _>(LoopbackGuest).client);
        let dir = temp_dir();
        let sock = dir.path().join("cli.sock");
        let server = tokio::task::spawn_local(cli_server::serve(
            sock.clone(),
            Supervisor::from_client(supervisor),
            vec![],
            rules,
        ));
        eventually_async("cli socket", async || sock.exists().then_some(())).await;
        Self {
            _dir: dir,
            sock,
            server,
            network,
        }
    }

    /// Run the forward command against this sandbox.
    /// Returns:
    ///   Sender that stops the command, and the command result. The result
    ///   comes after the forwards are active or after the command fails.
    async fn forward(
        &self,
        mode: Mode,
        forwards: &[PortMapping],
    ) -> (Running, Option<anyhow::Result<()>>) {
        let client = SandboxClient::connect_sock(&self.sock).await.unwrap();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        let report = Rc::new(Recorder {
            ready: RefCell::new(Some(ready_tx)),
            failures: RefCell::new(Vec::new()),
        });
        let forwards = forwards.to_vec();
        let task_report = report.clone();
        let mut task = tokio::task::spawn_local(async move {
            // Only a sent value stops the command. A dropped sender does not.
            let stop = async {
                if stop_rx.await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            forward(client, mode, &forwards, stop, task_report).await
        });
        let result = tokio::select! {
            biased;
            result = &mut task => Some(result.unwrap()),
            _ = ready_rx => None,
        };
        let running = Running {
            stop: stop_tx,
            task,
            report,
        };
        (running, result)
    }
}

/// Report that records the state changes of the forwards.
struct Recorder {
    ready: RefCell<Option<oneshot::Sender<()>>>,
    failures: RefCell<Vec<(u16, String)>>,
}

impl Report for Recorder {
    fn ready(&self, _mode: Mode, _forwards: &[PortMapping]) {
        let _ = self.ready.borrow_mut().take().unwrap().send(());
    }

    fn connect_failed(&self, guest_port: u16, reason: &str) {
        self.failures
            .borrow_mut()
            .push((guest_port, reason.to_string()));
    }
}

/// A forward command that runs.
struct Running {
    stop: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
    report: Rc<Recorder>,
}

impl Running {
    /// Get the ports and reasons of the reported connect failures.
    fn failures(&self) -> Vec<(u16, String)> {
        self.report.failures.borrow().clone()
    }

    /// Stop the command as Ctrl+C does and get its result.
    async fn stop(self) -> anyhow::Result<()> {
        let _ = self.stop.send(());
        self.task.await.unwrap()
    }
}

/// Send `text` to `127.0.0.1:<port>` and read the answer of the same
/// length.
/// Returns:
///   The answer, or `None` if the connection closes first.
async fn roundtrip(port: u16, text: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    stream.write_all(text.as_bytes()).await.ok()?;
    let mut buf = vec![0u8; text.len()];
    stream.read_exact(&mut buf).await.ok()?;
    Some(String::from_utf8(buf).unwrap())
}

/// Check if `127.0.0.1:<port>` is free.
fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Make a forward from `host` to `guest`.
fn mapping(host: u16, guest: u16) -> PortMapping {
    PortMapping { host, guest }
}

/// Test that a forward to the sandbox relays bytes from a host port to a
/// sandbox port, and that it reports a failure only when the forward
/// starts to fail. Then a real fault shows, but a series of failures shows
/// only one time.
///   1. Forward a free host port to a sandbox port with no listener
///   2. Connect two times and check that one failure is reported
///   3. Start an echo server on the sandbox port and check the echo
///   4. Stop the echo server, connect and check a second failure report
///   5. Stop the command and check that the host port is free
#[test]
fn guest_mode_forward_relays_bytes_and_reports_new_failures() {
    block_on_local(async {
        let sandbox = Sandbox::start("").await;
        let guest_port = free_port();
        let host_port = free_port();

        let (running, failed) = sandbox
            .forward(Mode::Guest, &[mapping(host_port, guest_port)])
            .await;
        assert!(failed.is_none(), "{failed:?}");
        assert_eq!(roundtrip(host_port, "ping").await, None);
        assert_eq!(roundtrip(host_port, "ping").await, None);
        assert_eq!(running.failures().len(), 1);

        let echo = serve_echo(TcpListener::bind(("127.0.0.1", guest_port)).await.unwrap());
        assert_eq!(roundtrip(host_port, "ping").await.as_deref(), Some("ping"));
        echo.abort();
        let _ = echo.await;
        assert_eq!(roundtrip(host_port, "ping").await, None);
        assert_eq!(running.failures().len(), 2);

        running.stop().await.unwrap();
        assert!(port_is_free(host_port));
    });
}

/// Test that a forward to the sandbox refuses a sandbox port that a config
/// forward sends back to the host, and that the command stays active and
/// reports the loop.
///   1. Configure a sandbox → host forward on a sandbox port
///   2. Forward a free host port to the same sandbox port
///   3. Connect and check that the connection closes
///   4. Check that the loop error is reported for the sandbox port
#[test]
fn guest_mode_forward_to_config_host_port_reports_loop() {
    block_on_local(async {
        let guest_port = free_port();
        let host_port = free_port();
        let sandbox =
            Sandbox::start(&format!("[network.ports.app]\nhost = [{guest_port}]\n")).await;

        let (running, failed) = sandbox
            .forward(Mode::Guest, &[mapping(host_port, guest_port)])
            .await;
        assert!(failed.is_none(), "{failed:?}");
        assert_eq!(roundtrip(host_port, "ping").await, None);
        assert_eq!(
            running.failures(),
            [(
                guest_port,
                messages::network_forward::loop_to_host(guest_port)
            )]
        );
    });
}

/// Test that a forward to the host relays bytes from a sandbox port to a
/// host port, and that the live network policy applies to each new
/// connection, the same as for config port forwards.
///   1. Start an echo server on a host port
///   2. Forward a free sandbox port to it
///   3. Send bytes to the sandbox port and check the echo
///   4. Change the policy to deny-always
///   5. Check that a new connection closes with no echo
///   6. Stop the command and check that the sandbox port is free
///   7. Repeat the forward on the public-only network of the install boot
///      and check that a connection closes with no echo
#[test]
fn host_mode_forward_relays_bytes_until_policy_denies() {
    block_on_local(async {
        let sandbox = Sandbox::start("").await;
        let host_port = echo_server().await;
        let guest_port = free_port();

        let (running, failed) = sandbox
            .forward(Mode::Host, &[mapping(host_port, guest_port)])
            .await;
        assert!(failed.is_none(), "{failed:?}");
        assert_eq!(roundtrip(guest_port, "ping").await.as_deref(), Some("ping"));

        sandbox.network.set_policy(Policy::DenyAlways);
        assert_eq!(roundtrip(guest_port, "ping").await, None);

        running.stop().await.unwrap();
        eventually_async("sandbox port free", async || {
            port_is_free(guest_port).then_some(())
        })
        .await;

        let install = Sandbox::start_with("", true).await;
        let (_running, failed) = install
            .forward(Mode::Host, &[mapping(host_port, guest_port)])
            .await;
        assert!(failed.is_none(), "{failed:?}");
        assert_eq!(roundtrip(guest_port, "ping").await, None);
    });
}

/// Test that one port conflict fails all forwards of the command, and no
/// forward stays active, in both modes.
///   1. Hold one port, and request a forward on a free port and on the
///      held port
///   2. Check that the command fails
///   3. Check that the free port is still free
#[test]
fn port_conflict_fails_all_forwards() {
    block_on_local(async {
        let sandbox = Sandbox::start("").await;
        for mode in [Mode::Guest, Mode::Host] {
            let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let held_port = held.local_addr().unwrap().port();
            let free = free_port();
            let target = free_port();
            let forwards = match mode {
                Mode::Guest => [mapping(free, target), mapping(held_port, target)],
                Mode::Host => [mapping(target, free), mapping(target, held_port)],
            };

            let (_running, failed) = sandbox.forward(mode, &forwards).await;
            assert!(matches!(failed, Some(Err(_))), "{mode:?}: {failed:?}");
            eventually_async("first port free", async || port_is_free(free).then_some(())).await;
        }
    });
}

/// Test that the sandbox refuses a forward that makes a loop with a config
/// forward of the other direction.
///   1. Configure a host → sandbox forward to a sandbox port
///   2. Request a sandbox → host forward on the same sandbox port
///   3. Check that the command fails with the loop error
#[test]
fn host_mode_forward_on_config_guest_port_is_refused() {
    block_on_local(async {
        let port = free_port();
        let sandbox = Sandbox::start(&format!("[network.ports.web]\nguest = [{port}]\n")).await;

        let (_running, failed) = sandbox.forward(Mode::Host, &[mapping(port, port)]).await;
        let err = format!("{:#}", failed.unwrap().unwrap_err());
        assert!(
            err.contains(&messages::network_forward::loop_from_host(port)),
            "{err}"
        );
    });
}

/// Test that the command ends with an error when the sandbox stops. The
/// sandbox shutdown aborts the CLI server.
///   1. Start a forward to the host
///   2. Abort the CLI server
///   3. Check that the command ends with the sandbox-stopped error
#[test]
fn sandbox_stop_ends_forward() {
    block_on_local(async {
        let sandbox = Sandbox::start("").await;
        let (running, failed) = sandbox
            .forward(Mode::Host, &[mapping(free_port(), free_port())])
            .await;
        assert!(failed.is_none(), "{failed:?}");

        sandbox.server.abort();
        let err = running.task.await.unwrap().unwrap_err();
        assert_eq!(err.to_string(), messages::network_forward::SANDBOX_STOPPED);
    });
}
