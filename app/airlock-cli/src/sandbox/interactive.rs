//! Interactive sandbox.
//!
//! Runs the sandbox of `airlock start` on the user's terminal, with raw
//! terminal passthrough or the monitor TUI.

use tracing::{error, info};

use super::Interrupted;
use super::boot::{BootOptions, BootSpec, boot, guest_env};
use super::io::drive;
use super::vm::{ProcessSpec, Stopped, Vm};
use crate::cli::{self, LogLevel};
use crate::network::{self, Network};
use crate::oci::{self, OciImage};
use crate::project::Project;
use crate::rpc::browser::Browser;
use crate::rpc::clipboard;
use crate::runtime::{DumpSink, HostRuntime, Runtime, Terminal};
use crate::services::{self, Services};
use crate::{daemon, masking};

/// Run an interactive sandbox session.
///
/// Boots `project` with all access that its config gives and runs `argv`
/// as the main process on the user's terminal. Serves `airlock exec` while
/// the process runs, then stops the sandbox in order. With network services
/// enabled, the guest can open their sign-in pages on the host for the full
/// session (browser access). Refused requests are reported after the
/// session.
/// Args:
///  - `project`: The locked project
///  - `image`: The container image
///  - `argv`: Command of the main process
///  - `runtime`: Terminal mode: raw passthrough or the monitor TUI
///  - `log_level`: Guest log level
///
/// Returns:
///   The exit code for the command: the process exit code, the TUI
///   override, or 130 if the user interrupts the boot. If the VM stop is not
///   confirmed after the process exits, the error is shown and the process
///   exit code is still returned.
pub async fn run_interactive(
    project: Project,
    image: &OciImage,
    argv: Vec<String>,
    mut runtime: HostRuntime,
    log_level: LogLevel,
) -> anyhow::Result<i32> {
    let tls_client = network::native_tls_client();
    let services = services::build_enabled(
        &project.config.network.services,
        &project.context,
        &tls_client,
    );
    let browser = Browser::new(services.browser_grants());
    let container_home = oci::effective_container_home(&project, image);
    let network = Network::new(
        &project,
        &container_home,
        tls_client,
        services.interceptors(),
        services.denied_targets(),
    )?;
    let env = guest_env(&project, image, browser.is_some());
    let daemons = daemon::build_specs(&project, &env)?;
    let masks = masking::build_specs(&project)?;
    let clipboard = clipboard::for_config(&project.config.clipboard);
    let spec = BootSpec {
        project,
        image,
        options: BootOptions {
            log_level,
            quiet: false,
            project_share: true,
        },
        env,
        network,
        browser: browser.clone(),
        clipboard,
        daemons,
        masks,
    };
    let mut vm = match Box::pin(boot(spec)).await {
        Ok(vm) => vm,
        Err(e) if e.is::<Interrupted>() => return Ok(130), // 128 + SIGINT
        Err(e) => return Err(e.context("boot the sandbox VM")),
    };
    services.attach(&vm.guest_network());

    // Start the output sink before the main process starts. The raw runtime
    // enters raw mode. The monitor runtime starts the TUI thread.
    let launched = (|| {
        let (stdin, pty) = runtime.attach_stdin()?;
        let signals = runtime.signals()?;
        let terminal = runtime.launch(vm.project(), vm.network(), vm.supervisor().clone())?;
        anyhow::Ok((stdin, pty, signals, terminal))
    })();
    let (stdin, pty, signals, mut terminal) = match launched {
        Ok(l) => l,
        Err(e) => return Err(shut_down_after(vm, &services, e).await),
    };

    let main = ProcessSpec {
        argv,
        cwd: None,
        env: vec![],
        unset: vec![],
        stdin,
        pty,
    };
    let proc = match vm.spawn(main).await {
        Ok(proc) => proc,
        Err(e) => {
            let e = e.context("start the main process");
            return Err(shut_down_after(vm, &services, e).await);
        }
    };
    info!("vm process started");

    // Start the CLI server, so `airlock exec` can attach processes to this VM.
    if let Err(e) = vm.serve_cli() {
        return Err(shut_down_after(vm, &services, e).await);
    }

    vm.forward_signals(signals, proc.clone());
    // If AIRLOCK_PTY_DUMP=1, also write all guest output to
    // <sandbox_dir>/pty.dump for offline replay and diagnosis.
    let exit_code = {
        let mut sink = DumpSink::from_env(&mut terminal, &vm.project().sandbox_dir);
        drive(&proc, &mut sink).await
    };
    info!("vm process exited, code = {exit_code}");

    let final_code = terminal.exit(exit_code);
    info!("terminal exit, final code = {final_code}");
    services.detach().await;
    if let Some(browser) = &browser {
        for notice in browser.take_notices() {
            cli::log!("{} {notice}", cli::yellow("browser:"));
        }
    }

    // The command ran, so its exit code is the result, also if the VM stop
    // is not confirmed. That failure is reported, not returned.
    match Box::pin(vm.shutdown()).await {
        Ok(Stopped { project, synced }) => {
            info!("all done, exit (guest sync confirmed: {synced})");
            // The project holds no lock. The caller holds the sandbox lock
            // and releases it after this function returns.
            drop(project);
        }
        Err(e) => {
            error!("shutdown after the main process exited: {e:#}");
            cli::error!("{e:#}");
        }
    }
    Ok(final_code)
}

/// Detach `services` and stop `vm` after `error`.
/// Returns:
///   The error to report: `error`, or the shutdown error (with `error` as
///   context) if the shutdown fails, because the VM may still run.
async fn shut_down_after(vm: Vm, services: &Services, error: anyhow::Error) -> anyhow::Error {
    services.detach().await;
    match Box::pin(vm.shutdown()).await {
        Ok(_) => error,
        Err(stop) => stop.context(format!("{error:#}")),
    }
}
