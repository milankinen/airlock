use std::pin::Pin;

use async_stream::stream;
use futures::Stream;
use tokio::signal::unix::{SignalKind, signal};

// Linux signal numbers — the target is always the Linux VM,
// regardless of the host platform.
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGQUIT: i32 = 3;
const SIGTERM: i32 = 15;
const SIGUSR1: i32 = 10;
const SIGUSR2: i32 = 12;

pub fn signals() -> anyhow::Result<Pin<Box<dyn Stream<Item = i32>>>> {
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
