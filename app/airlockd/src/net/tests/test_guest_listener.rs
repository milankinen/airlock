use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::supervisor_capnp::local_connector;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::net::bind_host_port;
use crate::net::rpc_bridge::connect_to_guest_port;
use crate::test_cfg::{block_on_local, echo_server, eventually_async, free_port, rpc_loopback};

/// Host-side connector that opens each connection to a local TCP server.
struct TcpConnector {
    port: u16,
    /// The number of next connections that the connector refuses.
    refuse: Cell<usize>,
}

impl local_connector::Server for TcpConnector {
    async fn connect(
        self: Rc<Self>,
        params: local_connector::ConnectParams,
        mut results: local_connector::ConnectResults,
    ) -> Result<(), capnp::Error> {
        let refuse = self.refuse.get();
        if refuse > 0 {
            self.refuse.set(refuse - 1);
            return Err(capnp::Error::failed("connect refused".into()));
        }
        let client = params.get()?.get_client()?;
        let server = connect_to_guest_port(self.port, client)
            .await
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        results.get().set_server(server);
        Ok(())
    }
}

/// Test the full life of a guest listener for a guest to host forward. The
/// connector is a remote capability, as on the host. The handle stays
/// local: a loopback RPC keeps its bootstrap capability alive, so a drop
/// there does not release it.
///   1. Listen on a free guest port with a connector to a real echo server
///   2. Connect to the guest port and check that bytes go both ways
///   3. Listen again on the same port and check that the bind fails
///   4. Release the handle and check that the open connection closes
///   5. Check that the port is free again
#[test]
fn guest_listener_relays_to_host_and_closes_when_handle_released() {
    block_on_local(async {
        let echo_port = echo_server().await;
        let connector: local_connector::Client = capnp_rpc::new_client(TcpConnector {
            port: echo_port,
            refuse: Cell::new(0),
        });
        let connector: local_connector::Client = rpc_loopback(connector.client);
        let port = free_port();

        let handle = bind_host_port(port, connector.clone()).await.unwrap();

        let mut conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        conn.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        conn.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        let err = bind_host_port(port, connector.clone()).await.err().unwrap();
        assert!(
            err.to_string()
                .starts_with(&format!("bind 127.0.0.1:{port}: ")),
            "{err}"
        );

        drop(handle);
        // EOF or reset, both show that the connection is closed.
        let n = tokio::time::timeout(Duration::from_secs(10), conn.read(&mut buf))
            .await
            .expect("connection stays open after release")
            .unwrap_or(0);
        assert_eq!(n, 0);

        eventually_async("guest port is free", async || {
            TcpListener::bind(("127.0.0.1", port)).await.ok()
        })
        .await;
    });
}

/// Test that a failed host connect closes only the accepted guest socket.
/// The listener must stay up for the next connections.
///   1. Listen with a connector that refuses the first connection
///   2. Connect and check that the guest socket gets EOF
///   3. Connect again and check that bytes go both ways
#[test]
fn guest_listener_closes_socket_when_connector_fails_and_keeps_listening() {
    block_on_local(async {
        let echo_port = echo_server().await;
        let connector: local_connector::Client = capnp_rpc::new_client(TcpConnector {
            port: echo_port,
            refuse: Cell::new(1),
        });
        let connector: local_connector::Client = rpc_loopback(connector.client);
        let port = free_port();
        let _handle = bind_host_port(port, connector).await.unwrap();

        let mut conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut buf = [0u8; 4];
        // EOF or reset, both show that the connection is closed.
        let n = tokio::time::timeout(Duration::from_secs(10), conn.read(&mut buf))
            .await
            .expect("refused connection stays open")
            .unwrap_or(0);
        assert_eq!(n, 0);

        let mut conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        conn.write_all(b"ping").await.unwrap();
        conn.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
    });
}
