//! Merges multiple Unix signals into a single async stream.
//!
//! The signal numbers emitted are Linux signal numbers (not host numbers),
//! because they are forwarded to the Linux VM process.

use async_stream::stream;
use tokio::signal::unix::{SignalKind, signal};

// Linux signal numbers — the target is always the Linux VM,
// regardless of the host platform.
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGQUIT: i32 = 3;
const SIGTERM: i32 = 15;
const SIGUSR1: i32 = 10;
const SIGUSR2: i32 = 12;

/// Create a stream that yields Linux signal numbers when the host receives
/// SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, or SIGUSR2.
pub fn signals() -> anyhow::Result<super::SignalStream> {
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigquit = signal(SignalKind::quit())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigusr1 = signal(SignalKind::user_defined1())?;
    let mut sigusr2 = signal(SignalKind::user_defined2())?;

    Ok(Box::pin(stream! {
        loop {
            yield tokio::select! {
                _ = sighup.recv() => SIGHUP,
                _ = sigint.recv() => SIGINT,
                _ = sigquit.recv() => SIGQUIT,
                _ = sigterm.recv() => SIGTERM,
                _ = sigusr1.recv() => SIGUSR1,
                _ = sigusr2.recv() => SIGUSR2,
            };
        }
    }))
}
