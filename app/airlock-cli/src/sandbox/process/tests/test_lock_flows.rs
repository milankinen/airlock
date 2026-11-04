//! Tests for the lock flows of the clients that get a sandbox process.
//! `airlock exec` and `airlock acp` attach to a running sandbox, wait for
//! a sandbox that starts, wait for a sandbox that stops before a new
//! sandbox process starts, and start only one sandbox process for free
//! locks. `airlock start` fails for a sandbox that runs, waits for a
//! sandbox that boots or stops, and attaches before the detach. The tests
//! use a real CLI server on a real Unix socket and the real sandbox locks.
//! A fake launcher takes the place of the sandbox process, so no VM is
//! necessary.

use std::os::unix::net::UnixListener;
use std::time::Duration;

use crate::messages;
use crate::sandbox::lock::{self, LockKind};
use crate::sandbox::process::client::{Located, Session, StartedProcess, ensure_with, start_with};
use crate::test_cfg::cli_server::{
    BOOT_FAILED, CliClient, FakeLauncher, TIMEOUT, assert_still_waits, hold_lock, serve_located,
    stopped_located,
};
use crate::test_cfg::{block_on_local, eventually_async};

/// Get a session in `located` as `airlock exec` does, with `launcher`.
async fn start_session_with(
    located: &Located,
    launcher: &FakeLauncher,
    timeout: Duration,
) -> anyhow::Result<Session> {
    Ok(ensure_with(located, launcher, timeout).await?.0)
}

/// Start the sandbox process of `located` as `airlock start` does. The
/// connect step gives the connection count that the sandbox sees before
/// the detach.
async fn start(
    located: &Located,
    launcher: &FakeLauncher,
) -> anyhow::Result<(Session, StartedProcess, u32)> {
    start_with(
        located,
        launcher,
        TIMEOUT,
        async || Ok(()),
        async |session| {
            let status = session.service.status_request().send().promise.await?;
            Ok(status.get()?.get_connections())
        },
    )
    .await
}

/// Test that the lookup attaches to a running sandbox and does not
/// start a second sandbox process.
///   1. Hold the run lock and serve the CLI socket as a running sandbox
///   2. Open a session and check that the sandbox counts it
///   3. Check that no sandbox process started
///   4. Drop the session and check that the sandbox stops
#[test]
fn running_sandbox_is_attached_without_start() {
    block_on_local(async {
        let (_tmp, located) = stopped_located();
        let _run = hold_lock(&located, LockKind::Run);
        let process = serve_located(&located).await;
        let launcher = FakeLauncher::default();

        let attached = start_session_with(&located, &launcher, TIMEOUT)
            .await
            .unwrap();
        assert_eq!(process.tracker.connections(), 1);
        assert_eq!(launcher.launches(), 0);

        drop(attached);
        eventually_async("sandbox stops", async || {
            process.tracker.is_stopping().then_some(())
        })
        .await;
    });
}

/// Test that the lookup waits for a sandbox process in its init, and
/// does not start a second one. The sandbox process holds the startup lock
/// until its parent connected.
///   1. Hold both locks with no socket, as a sandbox process in its init
///   2. Start the lookup and check that it waits
///   3. Serve the socket and free the startup lock, as the detach does
///   4. Check that the session opens on it and that no process started
#[test]
fn starting_sandbox_is_waited_for() {
    block_on_local(async {
        let (_tmp, located) = stopped_located();
        let startup = hold_lock(&located, LockKind::Startup);
        let _run = hold_lock(&located, LockKind::Run);
        let launcher = FakeLauncher::default();
        let open = start_session_with(&located, &launcher, TIMEOUT);
        let mut open = std::pin::pin!(open);

        assert_still_waits(open.as_mut(), "session opened with no sandbox").await;
        let process = serve_located(&located).await;
        drop(startup);
        let _attached = open.await.unwrap();
        assert_eq!(process.tracker.connections(), 1);
        assert_eq!(launcher.launches(), 0);
    });
}

/// Test that the lookup waits for the exit of a sandbox process that
/// stops, and then starts a new one. A session must not open on the old
/// sandbox, and the new process must not start while the old one holds
/// the run lock.
///   1. Hold the run lock and serve the socket as a sandbox that stops
///   2. Start the lookup and check that it waits and starts nothing
///   3. Stop the old server and check that the lookup still waits
///      for the run lock
///   4. Release the run lock, as the exit of the old process does
///   5. Check that one new process started and the session opened on it
#[test]
fn stopping_sandbox_is_waited_for_then_started_again() {
    block_on_local(async {
        let (_tmp, located) = stopped_located();
        let old_run = hold_lock(&located, LockKind::Run);
        let old = serve_located(&located).await;
        old.tracker.request_stop();
        let launcher = FakeLauncher::default();
        let open = start_session_with(&located, &launcher, TIMEOUT);
        let mut open = std::pin::pin!(open);

        assert_still_waits(open.as_mut(), "session opened on a sandbox that stops").await;
        assert_eq!(launcher.launches(), 0);
        assert_eq!(old.tracker.connections(), 0);

        // The abort removes the socket. The lock goes after it.
        old.server.abort();
        assert_still_waits(
            open.as_mut(),
            "session opened while the old process holds the lock",
        )
        .await;
        assert_eq!(launcher.launches(), 0);
        drop(old_run);
        let _attached = open.await.unwrap();
        assert_eq!(launcher.launches(), 1);
        assert_eq!(launcher.first_connections(), 1);
    });
}

/// Test that a sandbox process that closes each connection before it
/// answers does not end the lookup with an error. A sandbox that
/// stops closes new connections at once.
///   1. Hold the run lock, and accept and close each connection
///   2. Start the lookup and check that it waits
///   3. Stop the listener, remove the socket and release the run lock, as
///      the exit of the process does
///   4. Check that one process started and the session opened on it
#[test]
fn sandbox_that_closes_connections_is_waited_for() {
    block_on_local(async {
        let (_tmp, located) = stopped_located();
        let run = hold_lock(&located, LockKind::Run);
        let path = located.sock.to_path().unwrap().to_path_buf();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let closer = tokio::task::spawn_local(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        let launcher = FakeLauncher::default();
        let open = start_session_with(&located, &launcher, TIMEOUT);
        let mut open = std::pin::pin!(open);

        assert_still_waits(open.as_mut(), "lookup ended on a closing sandbox").await;
        closer.abort();
        std::fs::remove_file(&path).unwrap();
        drop(run);
        let _attached = open.await.unwrap();
        assert_eq!(launcher.launches(), 1);
        assert_eq!(launcher.first_connections(), 1);
    });
}

/// Test that two lookups at the same time on free locks and a stale
/// socket start only one sandbox process, and that both sessions open on
/// it. Only one of them gets the startup lock first. The other then finds
/// the sandbox. A sandbox that stopped by a crash leaves such a socket.
///   1. Leave a socket with no listener and free locks
///   2. Start two lookups at the same time
///   3. Check that one process started and that it has two connections
#[test]
fn concurrent_opens_on_free_locks_and_stale_socket_start_one_process() {
    block_on_local(async {
        let (_tmp, located) = stopped_located();
        // The socket file stays after the listener drops.
        UnixListener::bind(located.sock.to_path().unwrap()).unwrap();
        let launcher = FakeLauncher::default();

        let (first, second) = tokio::join!(
            start_session_with(&located, &launcher, TIMEOUT),
            start_session_with(&located, &launcher, TIMEOUT),
        );
        let (_first, _second) = (first.unwrap(), second.unwrap());
        assert_eq!(launcher.launches(), 1);
        assert_eq!(launcher.first_connections(), 2);
    });
}

/// Test that a sandbox process that fails gives its error, and that both
/// locks are free after it. A failed start must not block the next start.
///   1. Open a session with a sandbox process that fails
///   2. Check the error and that both locks are free
#[test]
fn failed_start_gives_error_and_frees_locks() {
    block_on_local(async {
        let (_tmp, located) = stopped_located();
        let launcher = FakeLauncher::failing();
        let Err(error) = start_session_with(&located, &launcher, TIMEOUT).await else {
            panic!("session opened with a failed sandbox process");
        };
        assert_eq!(error.to_string(), BOOT_FAILED);
        assert_eq!(launcher.launches(), 1);
        for kind in [LockKind::Startup, LockKind::Run] {
            crate::test_cfg::eventually("lock is free", || {
                (!lock::is_held(located.sandbox.dir(), kind)).then_some(())
            });
        }
    });
}

/// Test that `start` fails for a sandbox that a client uses, and that its
/// check does not stop that sandbox. `start` applies the config again, so
/// it must not use a sandbox that runs.
///   1. Hold the run lock and serve the socket as a running sandbox
///      process with one client
///   2. Start and check the error that names `airlock stop`
///   3. Check that no process started and that the sandbox still runs
///      with its one client
#[test]
fn start_while_running_fails() {
    block_on_local(async {
        let (_tmp, located) = stopped_located();
        let _run = hold_lock(&located, LockKind::Run);
        let process = serve_located(&located).await;
        let client = CliClient::connect(&process.sock).await;
        client.status().await.unwrap();
        let launcher = FakeLauncher::default();

        let Err(error) = start(&located, &launcher).await else {
            panic!("start used a sandbox that runs");
        };
        assert_eq!(error.to_string(), messages::start::START_ALREADY_RUNNING);
        assert_eq!(launcher.launches(), 0);
        crate::test_cfg::eventually_async("probe closed", async || {
            (process.tracker.connections() == 1).then_some(())
        })
        .await;
        assert!(!process.tracker.is_stopping());
    });
}

/// Test that `start` waits for a sandbox process in its init, and then
/// fails because the sandbox runs. A second `start` must not fail at once
/// while a different `exec` or `start` boots the sandbox, and must not
/// start a second sandbox process.
///   1. Hold both locks with no socket, as a sandbox process in its init
///   2. Start and check that it waits
///   3. Serve the socket with the connection of the parent and free the
///      startup lock, as the detach does
///   4. Check the error that names `airlock stop`, and that no process
///      started
#[test]
fn start_during_boot_waits_then_fails() {
    block_on_local(async {
        let (_tmp, located) = stopped_located();
        let startup = hold_lock(&located, LockKind::Startup);
        let _run = hold_lock(&located, LockKind::Run);
        let launcher = FakeLauncher::default();
        let started = start(&located, &launcher);
        let mut started = std::pin::pin!(started);

        assert_still_waits(started.as_mut(), "start ended during the boot").await;
        let process = serve_located(&located).await;
        let _parent = CliClient::connect(&process.sock).await;
        drop(startup);
        let Err(error) = started.await else {
            panic!("start used a sandbox that runs");
        };
        assert_eq!(error.to_string(), messages::start::START_ALREADY_RUNNING);
        assert_eq!(launcher.launches(), 0);
    });
}

/// Test that `start` waits for the exit of a sandbox process that stops,
/// and then starts a new one. A `start` directly after the last `exec`
/// must work, and must not start while the old process holds the disk.
///   1. Hold the run lock and serve the socket as a sandbox that stops
///   2. Start and check that it waits and starts nothing
///   3. Stop the old server and check that the start still waits for the
///      run lock
///   4. Release the run lock, as the exit of the old process does
///   5. Check that one new process started, and that the connect step ran
///      on its first connection before the detach
#[test]
fn start_during_stop_waits_then_starts() {
    block_on_local(async {
        let (_tmp, located) = stopped_located();
        let old_run = hold_lock(&located, LockKind::Run);
        let old = serve_located(&located).await;
        old.tracker.request_stop();
        let launcher = FakeLauncher::default();
        let started = start(&located, &launcher);
        let mut started = std::pin::pin!(started);

        assert_still_waits(started.as_mut(), "start ended on a sandbox that stops").await;
        assert_eq!(launcher.launches(), 0);
        // The abort removes the socket. The lock goes after it.
        old.server.abort();
        assert_still_waits(started.as_mut(), "start ended with a held lock").await;
        assert_eq!(launcher.launches(), 0);
        drop(old_run);

        let (_session, started, connections) = started.await.unwrap();
        assert_eq!(launcher.launches(), 1);
        assert_eq!(connections, 1);
        assert_eq!(started.ready.main_argv, Some(vec!["true".to_string()]));
        assert_eq!(launcher.first_connections(), 1);
    });
}
