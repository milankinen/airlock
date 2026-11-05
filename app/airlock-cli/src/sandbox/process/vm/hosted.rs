//! Hosted sandbox.
//!
//! Runs the sandbox of the sandbox process. The sandbox has all the access
//! that its config gives, but no main process. Clients attach their
//! processes over the CLI socket and hold the sandbox with their
//! connections. The sandbox stops when the last connection closes, when a
//! client requests a stop, when no connection comes in time, or on
//! SIGTERM. Browser notices go to the clients. The user approves requests
//! from the sandbox on the host desktop, or in the monitor of a client.

use std::rc::Rc;
use std::time::Duration;

use tracing::{error, info};

use super::Interrupted;
use super::boot::{BootOptions, boot};
use super::machine::Stopped;
use super::session::{SessionBoot, session_boot, shut_down_after};
use crate::approval::{self, MonitorSlot};
use crate::cli::{self, LogLevel};
use crate::messages;
use crate::oci::OciImage;
use crate::rpc::browser::Browser;
use crate::sandbox::Project;
use crate::sandbox::process::server as cli_server;
use crate::sandbox::process::server::connections::ConnectionTracker;
use crate::vm::VmmBackend;

/// Time limit for the first connection after the detach. The parent
/// connects before the detach, so this is only a safety net.
const FIRST_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Output and VM backend options of the hosted sandbox.
pub struct HostedOptions {
    /// Guest log level.
    pub log_level: LogLevel,
    /// Do not print the boot progress lines.
    pub quiet: bool,
    /// Connected socket of the parent of the sandbox process. It is the
    /// first connection of the CLI server.
    pub first_client: Option<std::os::unix::net::UnixStream>,
    /// The VM backend that boots the VM.
    pub vmm: &'static dyn VmmBackend,
}

/// Run the hosted sandbox until it stops.
///
/// Boots `project` with all the access that its config gives and serves
/// the CLI socket. Then calls `ready`, and waits for the stop. The stop
/// detaches the services and stops the VM. Shutdown errors go to the log
/// and to the exit code, because the clients are gone then.
/// Args:
///  - `project`: The locked project
///  - `image`: The container image
///  - `options`: Output options
///  - `ready`: Called one time when clients can connect. An error stops
///    the sandbox at once
///
/// Returns:
///   0 after the stop, 1 if the VM stop is not confirmed or `ready` fails,
///   or 130 if a signal interrupts the boot or `ready`. Error if the boot
///   or the CLI socket fails. Then `ready` was not called.
pub async fn run_hosted(
    project: Project,
    image: &OciImage,
    options: HostedOptions,
    ready: impl AsyncFnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<i32> {
    // The sandbox process runs the desktop dialogs, so it uses the desktop
    // env of the process that started it. A monitor client that attaches
    // fills the slot.
    let approvals = MonitorSlot::default();
    let approver = approval::for_settings(
        project.context.settings.approvals.dialogs,
        &project.context.data,
        Some(approvals.clone()),
    );
    let SessionBoot {
        spec,
        services,
        browser,
    } = session_boot(
        project,
        image,
        BootOptions {
            log_level: options.log_level,
            quiet: options.quiet,
            project_share: true,
            vmm: options.vmm,
        },
        approver,
    )?;
    let mut vm = match Box::pin(boot(spec)).await {
        Ok(vm) => vm,
        Err(e) if e.is::<Interrupted>() => return Ok(130), // 128 + SIGINT
        Err(e) => return Err(e.context(messages::sandbox::BOOT_SANDBOX_VM)),
    };
    services.attach(&vm.guest_network());

    let tracker = ConnectionTracker::new();
    let notices = notice_source(browser.as_ref());
    if let Err(e) = vm.serve_cli(tracker.clone(), notices, approvals, options.first_client) {
        return Err(shut_down_after(vm, &services, e).await);
    }
    info!("sandbox ready");
    // The exit code when the ready step fails: the parent or a signal
    // stopped the start.
    let failed_code = match ready().await {
        Ok(()) => None,
        Err(e) => {
            info!("sandbox stops before the detach: {e:#}");
            tracker.request_stop();
            Some(if e.is::<Interrupted>() { 130 } else { 1 })
        }
    };

    tokio::select! {
        reason = tracker.wait_stop(FIRST_CONNECTION_TIMEOUT) => {
            info!("sandbox stops: {reason:?}");
        }
        () = cli::interrupted() => {
            info!("sandbox stops: signal");
            tracker.request_stop();
        }
    }

    services.detach().await;
    // No client takes the notices after the stop. Keep them in the log.
    if let Some(browser) = &browser {
        for notice in browser.take_notices() {
            info!("browser notice not shown: {notice}");
        }
    }
    match Box::pin(vm.shutdown()).await {
        Ok(Stopped { project, synced }) => {
            info!("sandbox stopped (guest sync confirmed: {synced})");
            // The caller holds the sandbox lock and releases it at exit.
            drop(project);
            Ok(failed_code.unwrap_or(0))
        }
        Err(e) => {
            // The exit code tells `airlock start` that the stop failed.
            error!("sandbox shutdown: {e:#}");
            Ok(1)
        }
    }
}

/// Make the source of the browser notices for the clients. Each call takes
/// the new notices of `browser`.
fn notice_source(browser: Option<&Browser>) -> cli_server::NoticeSource {
    match browser.cloned() {
        Some(browser) => Rc::new(move || browser.take_notices()),
        None => cli_server::no_notices(),
    }
}
