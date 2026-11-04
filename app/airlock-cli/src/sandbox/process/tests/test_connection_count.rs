//! Tests for the connection count of the CLI server: the sandbox stops when
//! the last connection closes or on a stop request, and a port forward
//! does not hold it. The tests use a real Unix socket and real TCP. Only
//! the guest is fake, and a small loop takes the place of the sandbox
//! process: it waits for the stop and then aborts the server.

use std::time::Duration;

use tokio::task::JoinHandle;

use crate::sandbox::ForwardMode;
use crate::sandbox::process::server::connections::StopReason;
use crate::test_cfg::cli_server::{
    CliClient, CliSandbox, LoopbackGuest, NO_LIMIT, mapping, roundtrip, serve_located_with_parent,
    stopped_located,
};
use crate::test_cfg::oci::verified_dir;
use crate::test_cfg::{block_on_local, echo_server, eventually_async, free_port, temp_dir};
use crate::{messages, rpc};

/// Serve the CLI socket of a sandbox with no config and no connection.
/// Returns:
///   The temp dir of the socket, and the sandbox.
async fn start() -> (crate::test_cfg::TempDir, CliSandbox) {
    let dir = temp_dir();
    let sock = verified_dir(dir.path()).join("cli.sock").unwrap();
    let sandbox = CliSandbox::start_at(sock, "", false, LoopbackGuest::client()).await;
    (dir, sandbox)
}

/// Run the stop loop of the sandbox process: wait for the stop decision,
/// then shut down the server. The shutdown closes all connections and
/// removes the socket.
/// Returns:
///   The task. Its result is the stop reason.
fn run_process(sandbox: &CliSandbox) -> JoinHandle<StopReason> {
    let tracker = sandbox.tracker.clone();
    let server = sandbox.server.abort_handle();
    tokio::task::spawn_local(async move {
        let reason = tracker.wait_stop(NO_LIMIT).await;
        server.abort();
        reason
    })
}

/// Wait for the stop of the sandbox process, with a time limit.
async fn stopped(process: JoinHandle<StopReason>) -> StopReason {
    tokio::time::timeout(Duration::from_secs(5), process)
        .await
        .expect("sandbox did not stop")
        .unwrap()
}

/// Test that the sandbox stops when the last connection closes, not
/// before, and that a connection while it stops is closed at once. A
/// client must not attach to a sandbox that stops.
///   1. Connect two clients and check the status
///   2. Disconnect the first and check that the sandbox runs with one
///      connection
///   3. Disconnect the second and check the stop decision
///   4. Connect a new client and check that its call fails with a closed
///      connection
///   5. Run the stop loop and check that the socket goes
#[test]
fn last_connection_close_stops_and_new_connection_is_closed() {
    block_on_local(async {
        let (_dir, sandbox) = start().await;
        let first = CliClient::connect(&sandbox.sock).await;
        let second = CliClient::connect(&sandbox.sock).await;
        assert_eq!(second.status().await.unwrap(), (2, false));

        first.disconnect();
        eventually_async("first connection closed", async || {
            (sandbox.tracker.connections() == 1).then_some(())
        })
        .await;
        assert!(!sandbox.tracker.is_stopping());
        assert_eq!(second.status().await.unwrap(), (1, false));

        second.disconnect();
        eventually_async("stop decision", async || {
            sandbox.tracker.is_stopping().then_some(())
        })
        .await;
        let late = CliClient::connect(&sandbox.sock).await;
        let err = late.status().await.unwrap_err();
        assert!(rpc::is_closed(&err), "{err}");
        assert_eq!(sandbox.tracker.connections(), 0);

        let process = run_process(&sandbox);
        assert_eq!(stopped(process).await, StopReason::LastClosed);
        eventually_async("socket removed", async || {
            sandbox.sock.symlink_metadata().is_err().then_some(())
        })
        .await;
    });
}

/// Test that a port forward does not hold the sandbox, and that the
/// shutdown closes it. A forward must never keep a sandbox alive.
///   1. Connect a client
///   2. Start a forward to a host echo server and check the echo
///   3. Check that the status shows only the client connection
///   4. Disconnect the client and check that the sandbox stops while the
///      forward is active
///   5. Check that the forward command ends with the sandbox-stopped error
#[test]
fn unref_forward_does_not_hold_sandbox_and_closes_at_shutdown() {
    block_on_local(async {
        let (_dir, sandbox) = start().await;
        let process = run_process(&sandbox);
        let client = CliClient::connect(&sandbox.sock).await;

        let guest_port = free_port();
        let (running, failed) = sandbox
            .forward(ForwardMode::Host, &[mapping(echo_server().await, guest_port)])
            .await;
        assert!(failed.is_none(), "{failed:?}");
        assert_eq!(roundtrip(guest_port, "ping").await.as_deref(), Some("ping"));
        assert_eq!(client.status().await.unwrap(), (1, false));

        client.disconnect();
        assert_eq!(stopped(process).await, StopReason::LastClosed);
        let err = running.task.await.unwrap().unwrap_err();
        assert_eq!(err.to_string(), messages::network_forward::SANDBOX_STOPPED);
    });
}

/// Test that a stop request stops the sandbox while connections are open,
/// and that the shutdown closes them.
///   1. Connect a client
///   2. Call `stop` on another connection
///   3. Check that the sandbox stops for the request
///   4. Check that a call on the first connection fails
#[test]
fn stop_rpc_closes_open_connections() {
    block_on_local(async {
        let (_dir, sandbox) = start().await;
        let process = run_process(&sandbox);
        let client = CliClient::connect(&sandbox.sock).await;

        let control = CliClient::connect(&sandbox.sock).await;
        control.service.stop_request().send().promise.await.unwrap();
        assert_eq!(stopped(process).await, StopReason::Requested);

        assert!(client.status().await.is_err());
    });
}

/// Test that the first connection of the parent counts before any client
/// of the socket file. A client that connects and disconnects during the
/// start must not stop the sandbox.
///   1. Serve a sandbox with the parent on its first connection
///   2. Connect and disconnect a client of the socket file
///   3. Check that the sandbox runs with the parent connection
///   4. Drop the parent session and check the stop decision
#[test]
fn parent_first_connection_counts_before_socket_clients() {
    block_on_local(async {
        let (_tmp, located) = stopped_located();
        let (sandbox, parent) = serve_located_with_parent(&located).await;
        let client = CliClient::connect(&sandbox.sock).await;
        assert_eq!(client.status().await.unwrap(), (2, false));
        client.disconnect();
        eventually_async("client connection closed", async || {
            (sandbox.tracker.connections() == 1).then_some(())
        })
        .await;
        assert!(!sandbox.tracker.is_stopping());

        drop(parent);
        eventually_async("stop decision", async || {
            sandbox.tracker.is_stopping().then_some(())
        })
        .await;
    });
}

/// Test that `unref` tells if the end of the hold stops the sandbox. The
/// main command of `airlock start` uses it to decide if it waits for the
/// stop.
///   1. Connect two clients
///   2. Unref the first and check that the sandbox does not stop
///   3. Unref the second and check that the sandbox stops
#[test]
fn unref_tells_if_sandbox_stops() {
    block_on_local(async {
        let (_dir, sandbox) = start().await;
        let first = CliClient::connect(&sandbox.sock).await;
        let second = CliClient::connect(&sandbox.sock).await;
        assert_eq!(second.status().await.unwrap(), (2, false));

        assert!(!first.unref().await.unwrap());
        assert!(second.unref().await.unwrap());
        assert!(sandbox.tracker.is_stopping());
    });
}
