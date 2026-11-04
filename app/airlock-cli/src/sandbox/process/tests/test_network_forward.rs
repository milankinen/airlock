//! Tests for `airlock network forward`: the command and the CLI server of
//! `airlock start`, over a real Unix socket and real TCP. Only the guest is
//! fake: its loopback is the host loopback.

use tokio::net::TcpListener;

use crate::sandbox::ForwardMode;
use crate::config::config_values::Policy;
use crate::messages;
use crate::test_cfg::cli_server::{CliSandbox, LoopbackGuest, mapping, port_is_free, roundtrip};
use crate::test_cfg::{block_on_local, echo_server, eventually_async, free_port, serve_echo};

/// Serve the CLI socket for a sandbox with the config `toml` and the
/// loopback guest. With `public_only`, the sandbox has the network of the
/// install boot.
async fn start(toml: &str, public_only: bool) -> CliSandbox {
    CliSandbox::start(toml, public_only, LoopbackGuest::client()).await
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
        let sandbox = start("", false).await;
        let guest_port = free_port();
        let host_port = free_port();

        let (running, failed) = sandbox
            .forward(ForwardMode::Guest, &[mapping(host_port, guest_port)])
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
        let sandbox = start(
            &format!("[network.ports.app]\nhost = [{guest_port}]\n"),
            false,
        )
        .await;

        let (running, failed) = sandbox
            .forward(ForwardMode::Guest, &[mapping(host_port, guest_port)])
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
        let sandbox = start("", false).await;
        let host_port = echo_server().await;
        let guest_port = free_port();

        let (running, failed) = sandbox
            .forward(ForwardMode::Host, &[mapping(host_port, guest_port)])
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

        let install = start("", true).await;
        let (_running, failed) = install
            .forward(ForwardMode::Host, &[mapping(host_port, guest_port)])
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
        let sandbox = start("", false).await;
        for mode in [ForwardMode::Guest, ForwardMode::Host] {
            let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let held_port = held.local_addr().unwrap().port();
            let free = free_port();
            let target = free_port();
            let forwards = match mode {
                ForwardMode::Guest => [mapping(free, target), mapping(held_port, target)],
                ForwardMode::Host => [mapping(target, free), mapping(target, held_port)],
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
        let sandbox = start(&format!("[network.ports.web]\nguest = [{port}]\n"), false).await;

        let (_running, failed) = sandbox.forward(ForwardMode::Host, &[mapping(port, port)]).await;
        let err = format!("{:#}", failed.unwrap().unwrap_err());
        assert!(
            err.contains(&messages::network_forward::loop_from_host(port)),
            "{err}"
        );
    });
}
