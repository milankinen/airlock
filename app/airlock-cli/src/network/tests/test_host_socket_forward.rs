//! Tests of the socket forward to a host Unix socket.

use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::Path;
use std::time::Duration;

use airlock_common::network_capnp::network_proxy;
use airlock_test_utils::temp_dir;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;

use crate::test_cfg::network::*;

/// The guest path of the socket forward.
const GUEST_SOCKET: &str = "/run/guest.sock";

/// The config of a socket forward from the host path `host`.
fn config(host: &str) -> String {
    format!("[network.sockets.s]\nhost = \"{host}:{GUEST_SOCKET}\"\n")
}

/// Bind a host socket at `path`.
fn bind(path: &Path) -> UnixListener {
    let listener = StdUnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    UnixListener::from_std(listener).unwrap()
}

/// Bind a new socket at a temp name and rename it over `path`. The old
/// socket stays open until the caller drops it, so the inodes differ.
fn swap(path: &Path) -> UnixListener {
    let tmp = path.with_extension("new");
    let listener = bind(&tmp);
    std::fs::rename(&tmp, path).unwrap();
    listener
}

/// Connect from the guest to the forward and send `hello`.
async fn send_hello(proxy: &network_proxy::Client) {
    let conn = TestConnection::socket(proxy, GUEST_SOCKET)
        .await
        .expect("the proxy accepts the socket");
    conn.send(b"hello").await;
}

/// Check if `listener` gets the `hello` of [`send_hello`] in 500 ms.
async fn got_hello(listener: &UnixListener) -> bool {
    let Ok(accepted) = tokio::time::timeout(Duration::from_millis(500), listener.accept()).await
    else {
        return false;
    };
    let (mut stream, _) = accepted.unwrap();
    let mut buf = vec![0u8; 64];
    let n = stream.read(&mut buf).await.unwrap();
    &buf[..n] == b"hello"
}

/// Test that the proxy refuses a socket in the project dir that was swapped
/// for an other socket after the start. The sandbox can write the project
/// dir. It must not send the guest connections to a socket that it picked.
///   1. Bind a socket in the project dir and make a network with a forward
///      to it
///   2. Connect from the guest and check that the socket gets the data
///   3. Rename a new socket over the path
///   4. Connect again and check that the new socket gets no connection
#[test]
fn host_socket_swapped_after_start_is_refused() {
    let dir = temp_dir();
    let path = dir.path().join("host.sock");
    let first = StdUnixListener::bind(&path).unwrap();
    first.set_nonblocking(true).unwrap();
    let network = network_in_project(&config("./host.sock"), dir.path())
        .unwrap()
        .network;

    run_network(network, |proxy| async move {
        let first = UnixListener::from_std(first).unwrap();
        send_hello(&proxy).await;
        assert!(got_hello(&first).await);

        let second = swap(&path);
        send_hello(&proxy).await;
        assert!(!got_hello(&second).await);
    });
}

/// Test that a socket in the project dir that is missing at the start is
/// pinned at the first connection. The host service can start after the
/// sandbox.
///   1. Make a network with a forward to a missing socket in the project
///      dir
///   2. Bind the socket, connect and check that the socket gets the data
///   3. Rename a new socket over the path, connect again, and check that
///      the new socket gets no connection
#[test]
fn socket_missing_at_start_is_pinned_at_first_connect() {
    let dir = temp_dir();
    let path = dir.path().join("host.sock");
    let network = network_in_project(&config("./host.sock"), dir.path())
        .unwrap()
        .network;

    run_network(network, |proxy| async move {
        let first = bind(&path);
        send_hello(&proxy).await;
        assert!(got_hello(&first).await);

        let second = swap(&path);
        send_hello(&proxy).await;
        assert!(!got_hello(&second).await);
    });
}

/// Test that a socket path through a symlink in the project dir to a socket
/// outside of it is refused at the start. The sandbox can change the
/// symlinks in the project dir.
///   1. Bind a socket outside of the project dir
///   2. Make a symlink to it in the project dir
///   3. Check that the network with a forward to the symlink fails
#[test]
fn symlink_out_of_project_dir_is_refused_at_start() {
    let dir = temp_dir();
    let other = temp_dir();
    let _listener = StdUnixListener::bind(other.path().join("host.sock")).unwrap();
    symlink(other.path().join("host.sock"), dir.path().join("link.sock")).unwrap();

    let Err(e) = network_in_project(&config("./link.sock"), dir.path()) else {
        panic!("the network accepts the symlink");
    };
    assert!(format!("{e:#}").contains("sandbox can write"), "{e:#}");
}

/// Test that a symlink that appears at the socket path after the start is
/// refused. A symlink can point to any host socket.
///   1. Bind a socket and make a network with a forward to it
///   2. Replace the socket with a symlink to an other socket
///   3. Connect and check that the other socket gets no connection
#[test]
fn symlink_planted_after_start_is_refused() {
    let dir = temp_dir();
    let other = temp_dir();
    let path = dir.path().join("host.sock");
    let _first = StdUnixListener::bind(&path).unwrap();
    let network = network_in_project(&config("./host.sock"), dir.path())
        .unwrap()
        .network;

    run_network(network, |proxy| async move {
        let target = bind(&other.path().join("other.sock"));
        std::fs::remove_file(&path).unwrap();
        symlink(other.path().join("other.sock"), &path).unwrap();
        send_hello(&proxy).await;
        assert!(!got_hello(&target).await);
    });
}

/// Test that a socket outside of the dirs that the sandbox can write is not
/// pinned. A host service that restarts makes its socket again, and the
/// forward must still work.
///   1. Bind a socket outside of the project dir and make a network with a
///      forward to it
///   2. Rename a new socket over the path
///   3. Connect and check that the new socket gets the data
#[test]
fn socket_outside_writable_dirs_recreated_still_connects() {
    let dir = temp_dir();
    let other = temp_dir();
    let path = other.path().join("host.sock");
    let network = network_in_project(&config(&path.display().to_string()), dir.path())
        .unwrap()
        .network;

    run_network(network, |proxy| async move {
        let first = bind(&path);
        send_hello(&proxy).await;
        assert!(got_hello(&first).await);

        let second = swap(&path);
        send_hello(&proxy).await;
        assert!(got_hello(&second).await);
    });
}
