//! Tests for the sandbox stop of `airlock stop`: the stop request closes
//! the connections, and the stop waits until the sandbox process released
//! the run lock. A
//! stop during the boot waits for the boot and then stops the sandbox. The
//! tests use a real CLI server on a real Unix socket and the real sandbox
//! locks. A small loop takes the place of the sandbox process. No VM is
//! necessary.

use std::time::Duration;

use crate::sandbox::StopResult;
use crate::sandbox::lock::{self, LockKind};
use crate::test_cfg::block_on_local;
use crate::test_cfg::cli_server::{
    CliClient, NO_LIMIT, assert_still_waits, hold_lock, registered_located, serve_located,
};

/// Time that the fake sandbox process takes to stop its VM after the stop
/// decision.
const VM_STOP: Duration = Duration::from_millis(300);

/// Test that the stop stops a sandbox that a client uses, and returns only
/// after the sandbox process released the run lock. Thus the disk is
/// synced when the stop ends.
///   1. Hold the run lock and serve the socket as a running sandbox
///      process with one client
///   2. Run the process loop: wait for the stop, stop the server, wait for
///      the VM stop and then release the lock
///   3. Stop the sandbox and check that it stopped
///   4. Check that the lock is free and that the connection closed
#[test]
fn stop_closes_connection_and_waits_for_lock_release() {
    let (_home, context, located) = registered_located();
    block_on_local(async {
        let lock = hold_lock(&located, LockKind::Run);
        let sandbox = serve_located(&located).await;
        let _client = CliClient::connect(&sandbox.sock).await;

        let tracker = sandbox.tracker.clone();
        let server = sandbox.server.abort_handle();
        let process = tokio::task::spawn_local(async move {
            tracker.wait_stop(NO_LIMIT).await;
            server.abort();
            tokio::time::sleep(VM_STOP).await;
            drop(lock);
        });

        assert_eq!(
            located.sandbox.stop(&context).await.unwrap(),
            StopResult::Stopped
        );
        assert!(process.is_finished());
        // A fork in a parallel test can hold a copy of the file for a
        // short time.
        crate::test_cfg::eventually("run lock is free", || {
            (!lock::is_held(located.sandbox.dir(), LockKind::Run)).then_some(())
        });
        assert_eq!(sandbox.tracker.connections(), 0);
    });
}

/// Test that the stop of a sandbox that does not run does nothing and
/// tells that the sandbox did not run, so that scripts can call it at any
/// time. It also leaves both locks free. A registered sandbox with no
/// directory also did not run.
///   1. Make a stopped sandbox
///   2. Stop the sandbox and check that it did not run, and that both
///      locks are free
///   3. Remove the sandbox directory, stop again and check that it did not
///      run
#[test]
fn stop_of_stopped_sandbox_succeeds() {
    let (_home, context, located) = registered_located();
    block_on_local(async {
        assert_eq!(
            located.sandbox.stop(&context).await.unwrap(),
            StopResult::NotRunning
        );
        // A fork in a parallel test can hold a copy of the file for a
        // short time.
        for kind in [LockKind::Startup, LockKind::Run] {
            crate::test_cfg::eventually("lock is free", || {
                (!lock::is_held(located.sandbox.dir(), kind)).then_some(())
            });
        }

        located.sandbox.dir().remove_dir_all().unwrap();
        assert_eq!(
            located.sandbox.stop(&context).await.unwrap(),
            StopResult::NotRunning
        );
    });
}

/// Test that the stop waits for a sandbox process in its init, and stops the
/// sandbox after the boot. A stop during the boot must not leave a sandbox
/// that runs.
///   1. Hold both locks with no socket, as a sandbox process in its init
///   2. Start the stop and check that it waits
///   3. Serve the socket with the connection of the parent, run the
///      process loop and free the startup lock, as the detach does
///   4. Check that it stopped, and that the run lock is free
#[test]
fn stop_during_boot_waits_and_stops_sandbox() {
    let (_home, context, located) = registered_located();
    block_on_local(async {
        let startup = hold_lock(&located, LockKind::Startup);
        let lock = hold_lock(&located, LockKind::Run);
        let stopping = located.sandbox.stop(&context);
        let mut stopping = std::pin::pin!(stopping);

        assert_still_waits(stopping.as_mut(), "stop ended during the boot").await;
        let sandbox = serve_located(&located).await;
        let parent = CliClient::connect(&sandbox.sock).await;
        parent.status().await.unwrap();
        let tracker = sandbox.tracker.clone();
        let server = sandbox.server.abort_handle();
        tokio::task::spawn_local(async move {
            tracker.wait_stop(NO_LIMIT).await;
            server.abort();
            tokio::time::sleep(VM_STOP).await;
            drop(lock);
        });
        drop(startup);

        assert_eq!(stopping.await.unwrap(), StopResult::Stopped);
        // A fork in a parallel test can hold a copy of the file for a
        // short time.
        crate::test_cfg::eventually("run lock is free", || {
            (!lock::is_held(located.sandbox.dir(), LockKind::Run)).then_some(())
        });
    });
}

/// Test that the stop waits for a sandbox process that stops and serves no
/// socket any more. The disk is synced only after its exit.
///   1. Hold the run lock with no socket, as a sandbox process that stops
///   2. Start the stop and check that it waits
///   3. Release the run lock
///   4. Check that it stopped
#[test]
fn stop_waits_for_stopping_sandbox() {
    let (_home, context, located) = registered_located();
    block_on_local(async {
        let lock = hold_lock(&located, LockKind::Run);
        let stopping = located.sandbox.stop(&context);
        let mut stopping = std::pin::pin!(stopping);

        assert_still_waits(stopping.as_mut(), "stop ended before the exit").await;
        drop(lock);
        assert_eq!(stopping.await.unwrap(), StopResult::Stopped);
    });
}
