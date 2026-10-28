//! Tests for `CliService.addMcpServer`: the CLI server of `airlock start`
//! over a real Unix socket. Only the guest is fake.

use std::rc::Rc;
use std::time::Duration;

use airlock_common::cli_capnp::cli_service;
use airlock_common::supervisor_capnp::{mcp_grant, mcp_host, mcp_session, mcp_sink};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::cli::sandbox_client::SandboxClient;
use crate::cli_server::{self, ForwardRules};
use crate::rpc::Supervisor;
use crate::test_cfg::mcp_grant::{GrantEvent, McpGuest, Route};
use crate::test_cfg::network::network_from_toml;
use crate::test_cfg::oci::verified_dir;
use crate::test_cfg::{TempDir, block_on_local, eventually_async, resolve_project_toml, temp_dir};
use crate::util::verified_path::VerifiedPath;

/// Host object of the test client. It answers each `open` with one message
/// and records what the guest sends to the sessions.
struct FakeHost {
    received: mpsc::UnboundedSender<String>,
}

impl mcp_host::Server for FakeHost {
    async fn open(
        self: Rc<Self>,
        params: mcp_host::OpenParams,
        mut results: mcp_host::OpenResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let name = params.get_name()?.to_str()?.to_string();
        let sink = params.get_sink()?;
        tokio::task::spawn_local(async move {
            let mut req = sink.message_request();
            req.get().set_message(format!("ready:{name}"));
            let _ = req.send().promise.await;
        });
        results
            .get()
            .set_session(capnp_rpc::new_client(FakeSession {
                received: self.received.clone(),
            }));
        Ok(())
    }
}

struct FakeSession {
    received: mpsc::UnboundedSender<String>,
}

impl mcp_session::Server for FakeSession {
    async fn send(
        self: Rc<Self>,
        params: mcp_session::SendParams,
        _: mcp_session::SendResults,
    ) -> Result<(), capnp::Error> {
        let _ = self
            .received
            .send(params.get()?.get_message()?.to_str()?.to_string());
        Ok(())
    }
}

/// Guest sink that records the messages from the host.
struct FakeSink {
    messages: mpsc::UnboundedSender<String>,
}

impl mcp_sink::Server for FakeSink {
    async fn message(
        self: Rc<Self>,
        params: mcp_sink::MessageParams,
        _: mcp_sink::MessageResults,
    ) -> Result<(), capnp::Error> {
        let _ = self
            .messages
            .send(params.get()?.get_message()?.to_str()?.to_string());
        Ok(())
    }
}

/// Client connection to the CLI server. Its drop ends the connection.
struct Client {
    service: cli_service::Client,
    rpc: JoinHandle<()>,
}

impl Client {
    async fn connect(sock: &VerifiedPath) -> Self {
        let SandboxClient {
            service,
            rpc_system,
        } = SandboxClient::connect_sock(sock).await.unwrap();
        let rpc = tokio::task::spawn_local(async move {
            let _ = rpc_system.await;
        });
        Self { service, rpc }
    }

    /// Call `addMcpServer` with a fresh fake host.
    /// Returns:
    ///   The grant, and the receiver of the messages that the guest sends
    ///   to the sessions of the host.
    async fn add(
        &self,
        session: &str,
        name: &str,
    ) -> Result<(mcp_grant::Client, mpsc::UnboundedReceiver<String>), capnp::Error> {
        let (tx, rx) = mpsc::unbounded_channel();
        let host: mcp_host::Client = capnp_rpc::new_client(FakeHost { received: tx });
        let mut req = self.service.add_mcp_server_request();
        req.get().set_session(session);
        req.get().set_name(name);
        req.get().set_host(host);
        let grant = req.send().promise.await?.get()?.get_grant()?;
        Ok((grant, rx))
    }

    /// End the connection.
    fn disconnect(self) {
        self.rpc.abort();
    }
}

/// CLI server of a fake running sandbox with a fake guest.
struct Sandbox {
    _dir: TempDir,
    sock: VerifiedPath,
    routes: mpsc::UnboundedReceiver<Route>,
    events: mpsc::UnboundedReceiver<GrantEvent>,
    _server: JoinHandle<()>,
}

impl Sandbox {
    async fn start() -> Self {
        let config = resolve_project_toml("").unwrap().values;
        let network = network_from_toml("").unwrap().network.handle().control();
        let rules = ForwardRules::new(&config.network, network);
        let (guest, routes, events) = McpGuest::start();
        let dir = temp_dir();
        let sock = verified_dir(dir.path()).join("cli.sock").unwrap();
        let server = tokio::task::spawn_local(cli_server::serve(
            sock.clone(),
            Supervisor::from_client(guest),
            vec![],
            rules,
        ));
        eventually_async("cli socket", async || {
            sock.symlink_metadata().is_ok().then_some(())
        })
        .await;
        Self {
            _dir: dir,
            sock,
            routes,
            events,
            _server: server,
        }
    }
}

/// Wait for the next value of `rx`, with a time limit.
async fn next<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed")
}

/// Test that the CLI server passes the host object to the guest, and that
/// the guest can use it through the server.
///   1. Add a server with a host object in the client
///   2. Check that the guest got the session, the name and a host object
///   3. Open a session from the guest and check the message that the
///      client sends to the guest sink
///   4. Send a message to the session and check that the client gets it
#[test]
fn add_mcp_server_gives_working_host_to_guest() {
    block_on_local(async {
        let mut sandbox = Sandbox::start().await;
        let client = Client::connect(&sandbox.sock).await;
        let (_grant, mut received) = client.add("sess-1_A", "files").await.unwrap();

        let route = next(&mut sandbox.routes).await;
        assert_eq!(
            (route.session.as_str(), route.name.as_str()),
            ("sess-1_A", "files")
        );

        let (messages_tx, mut messages) = mpsc::unbounded_channel();
        let mut open = route.host.open_request();
        open.get().set_name("files");
        open.get().set_sink(capnp_rpc::new_client(FakeSink {
            messages: messages_tx,
        }));
        let session = open
            .send()
            .promise
            .await
            .unwrap()
            .get()
            .unwrap()
            .get_session()
            .unwrap();
        assert_eq!(next(&mut messages).await, "ready:files");

        let mut send = session.send_request();
        send.get().set_message("ping");
        send.send().promise.await.unwrap();
        assert_eq!(next(&mut received).await, "ping");
    });
}

/// Test that an explicit grant release and a client disconnect both reach
/// the grant in the guest.
///   1. Add a server and call `release` on the grant
///   2. Check that the guest grant gets the release
///   3. Add a second server on a second connection
///   4. Disconnect the client without a release
///   5. Check that the guest grant is dropped
#[test]
fn grant_release_and_client_disconnect_reach_guest() {
    block_on_local(async {
        let mut sandbox = Sandbox::start().await;

        let client = Client::connect(&sandbox.sock).await;
        let (grant, _received) = client.add("s1", "one").await.unwrap();
        grant.release_request().send().promise.await.unwrap();
        assert_eq!(next(&mut sandbox.events).await, GrantEvent::Released);
        drop(grant);
        client.disconnect();
        // Drain the drop event of the first grant.
        assert_eq!(next(&mut sandbox.events).await, GrantEvent::Dropped);

        let client = Client::connect(&sandbox.sock).await;
        let (grant, _received) = client.add("s2", "two").await.unwrap();
        assert!(sandbox.events.try_recv().is_err());
        // The grant stays alive in the client until the connection ends.
        client.disconnect();
        assert_eq!(next(&mut sandbox.events).await, GrantEvent::Dropped);
        drop(grant);
    });
}

/// Test that the CLI server refuses session and name values that are not
/// valid in a route, and does not call the guest.
///   1. Add servers with an empty, a too long and a bad character value
///      (`/`, `.`, space) in the session, then in the name
///   2. Check that each call fails
///   3. Add a server with the longest valid values and check that it works
///   4. Check that the guest got only the valid call
#[test]
fn add_mcp_server_with_bad_session_or_name_is_refused() {
    block_on_local(async {
        let mut sandbox = Sandbox::start().await;
        let client = Client::connect(&sandbox.sock).await;

        let long = "a".repeat(65);
        for bad in ["", long.as_str(), "a/b", "a.b", "a b"] {
            assert!(client.add(bad, "ok").await.is_err(), "session {bad:?}");
            assert!(client.add("ok", bad).await.is_err(), "name {bad:?}");
        }

        let max = "a".repeat(64);
        let _ok = client.add(&max, "-_Z9").await.unwrap();
        let route = next(&mut sandbox.routes).await;
        assert_eq!((route.session, route.name), (max, "-_Z9".to_string()));
        assert!(sandbox.routes.try_recv().is_err());
    });
}
