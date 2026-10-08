//! Sandbox VM boot.
//!
//! Starts a VM with all the access that the configuration gives the guest:
//! mounts, networking, daemons and port forwards. After the boot, the guest
//! is ready, but no process runs.

use std::os::unix::io::OwnedFd;
use std::time::Duration;

use airlock_common::BROWSER_SHIM;
use anyhow::Context;
use tracing::info;

use super::report;
use super::tasks::BootTasks;
use super::vm::Vm;
use crate::cli::{self, LogLevel};
use crate::network::reverse_forward::BoundForward;
use crate::network::{self, Network, NetworkHandle};
use crate::oci::{self, OciImage};
use crate::project::Project;
use crate::rpc;
use crate::rpc::browser::Browser;
use crate::rpc::clipboard::ClipboardImpl;
use crate::rpc::guest_network::GuestNetwork;
use crate::vm::{self, VmInstance};

/// Interval of the host wall clock sync to the guest.
const CLOCK_SYNC_INTERVAL: Duration = Duration::from_mins(1);

/// Output and sharing options of one boot.
pub struct BootOptions {
    /// Guest kernel verbosity and the guest `tracing` filter.
    pub log_level: LogLevel,
    /// Do not print the boot progress lines (the verbose config report,
    /// "Booting VM..." and the VM resources summary). For callers that print
    /// their own progress.
    pub quiet: bool,
    /// Mount the project directory and start processes in it. If `false`,
    /// the project is not shared and the guest works in `/`.
    pub project_share: bool,
}

/// All that one boot gives the guest. The caller builds it. [`boot`] only
/// connects it to the VM.
pub struct BootSpec<'a> {
    /// The locked project to boot.
    pub project: Project,
    /// The container image.
    pub image: &'a OciImage,
    /// Output and sharing options.
    pub options: BootOptions,
    /// The sandbox env of all processes and daemons (see [`guest_env`]).
    pub env: Vec<String>,
    /// The sandbox network.
    pub network: Network,
    /// The browser bridge. `None` gives no browser access.
    pub browser: Option<Browser>,
    /// The clipboard bridge. `None` gives no clipboard access.
    pub clipboard: Option<ClipboardImpl>,
    /// The daemons to start in the guest.
    pub daemons: Vec<rpc::DaemonSpec>,
    /// The directory masks to apply in the guest.
    pub masks: Vec<rpc::MaskSpec>,
}

/// Build the sandbox env.
/// Args:
///  - `project`: Project with the `[env]` values
///  - `image`: Image with the base env
///  - `browser_shim`: `true` if the boot gives browser access
///
/// Returns:
///   The image env with the guest-visible `[env]` values on top. Masked
///   entries contain their surrogate, never the real value. With
///   `browser_shim`, the browser shim directory is first in `PATH` (so tools
///   that run `xdg-open` find the shim). Also, `BROWSER` is the shim path,
///   unless the user's `[env]` sets `BROWSER`.
pub fn guest_env(project: &Project, image: &OciImage, browser_shim: bool) -> Vec<String> {
    let env = crate::util::merge_env(&image.env, project.env.guest_entries(), &[]);
    if !browser_shim {
        return env;
    }
    let user_browser = project.config.env.contains_key("BROWSER");
    apply_browser_shim(&env, user_browser)
}

/// Boot a VM for `spec.project` with `spec.image`.
///
/// Binds the reverse port forwards, starts the VM, serves the supervisor and
/// network RPC, and boots the guest (mounts, networking, daemons, and the
/// access that `spec` gives). Does not start a process.
/// Returns:
///   The booted VM. Error [`super::Interrupted`] if the user interrupts
///   before or during the boot. If a failure occurs after the VM started,
///   the function stops the VM before it returns the error.
pub async fn boot(spec: BootSpec<'_>) -> anyhow::Result<Vm> {
    let BootSpec {
        project,
        image,
        options,
        env,
        network,
        browser,
        clipboard,
        daemons,
        masks,
    } = spec;

    if !options.quiet {
        report::print_mounts_and_rules(&project);
    }

    // Check if the user interrupted the setup (for example Ctrl+C during a
    // download).
    if cli::is_interrupted() {
        return Err(super::Interrupted.into());
    }

    // Bind the reverse port forward listeners before the VM boot. Then bind
    // errors (for example EADDRINUSE) show immediately, without the VM boot
    // output. The listeners stay open until the supervisor is ready. Then
    // accept loops start on them.
    let reverse_forwards = network::reverse_forward::bind(
        network::rules::reverse_port_forwards_from_config(&project.config.network),
    )
    .await?;

    if !options.quiet {
        cli::log!("Booting VM...");
    }
    let container_home = oci::effective_container_home(&project, image);
    let (instance, vsock_fd) = vm::start(&project, image, &container_home, env, &options).await?;
    project.save_meta();

    let mut tasks = BootTasks::default();
    let (supervisor, network, socket_fwds) =
        match connect(&instance, vsock_fd, network, reverse_forwards, &mut tasks).await {
            Ok(connected) => connected,
            Err(e) => {
                return match stop_vm(instance, tasks).await {
                    Ok(()) => Err(e),
                    Err(stop) => Err(stop.context(format!("boot failed: {e:#}"))),
                };
            }
        };

    let booted = supervisor
        .boot(rpc::BootRequest {
            project: &project,
            vm: &instance,
            log_filter: options.log_level.filter(),
            socket_fwds: &socket_fwds,
            daemons: &daemons,
            masks: &masks,
            browser,
            clipboard,
        })
        .await;
    if let Err(e) = booted {
        // No daemon runs after a failed boot. Thus the shutdown has no daemons
        // to stop.
        let vm = Vm::new(project, instance, supervisor, network, vec![], tasks);
        return match vm.shutdown().await {
            Ok(_) => Err(e),
            Err(stop) => Err(stop.context(format!("boot failed: {e:#}"))),
        };
    }
    info!("vm booted");
    // Send the host wall clock to the guest every minute, so the VM clock
    // stays correct after host sleep (laptop lid closed, suspend). VMs have
    // no RTC. Without this sync, the guest time is late by the sleep
    // duration.
    tasks.spawn_service(supervisor.clock_sync(CLOCK_SYNC_INTERVAL));
    let daemon_names = daemons.into_iter().map(|d| d.name).collect();
    Ok(Vm::new(
        project,
        instance,
        supervisor,
        network,
        daemon_names,
        tasks,
    ))
}

/// Connect the RPC channels of a started VM. The supervisor connection is
/// first. The guest accepts the network channel immediately after it.
/// Returns:
///   The supervisor client, the network handle and the socket forwards as
///   `(host path, guest path)` pairs.
async fn connect(
    vm: &VmInstance,
    vsock_fd: OwnedFd,
    network: Network,
    reverse_forwards: Vec<BoundForward>,
    tasks: &mut BootTasks,
) -> anyhow::Result<(rpc::Supervisor, NetworkHandle, Vec<(String, String)>)> {
    // A Ctrl+C during boot (the vsock connect can retry for ~12s) sets the
    // interrupt flag, but the boot path does not monitor it. Check it here,
    // before the supervisor setup. Then the caller stops the new VM and does
    // not boot a sandbox that the user already cancelled.
    if cli::is_interrupted() {
        info!("interrupted during boot; shutting down VM");
        return Err(super::Interrupted.into());
    }

    let (supervisor, driver) = rpc::Supervisor::connect(vsock_fd)?;
    tasks.spawn_transport(driver);
    tasks.spawn_service(network.deny_reporter().attach(supervisor.client()));

    // Connect the bound reverse port forward listeners to the supervisor,
    // which is now ready.
    let guest = GuestNetwork::new(supervisor.client());
    network::reverse_forward::serve(reverse_forwards, &guest, tasks.services());

    // Get the socket forward data and the handle before the NetworkProxy RPC
    // server takes ownership of `Network`.
    let socket_fwds = network
        .socket_map
        .iter()
        .map(|(guest, host)| (host.to_string_lossy().into_owned(), guest.clone()))
        .collect();
    let handle = network.handle();

    // Open the dedicated vsock for the NetworkProxy RPC and serve `Network`
    // as its bootstrap capability. Bulk byte relays then do not use the
    // supervisor channel, so they cannot block pty, stats and daemon traffic.
    let network_fd = vm.vsock_connect(airlock_common::NETWORK_PORT).await?;
    tasks.spawn_transport(rpc::serve_network(network_fd, network)?);

    Ok((supervisor, handle, socket_fwds))
}

/// Stop a boot that has no usable supervisor. Stops the services, then the
/// VM (with confirmation), then the transport.
async fn stop_vm(vm: VmInstance, mut tasks: BootTasks) -> anyhow::Result<()> {
    tasks.stop_services().await;
    info!("vm shutdown");
    let stopped = vm.shutdown().await;
    tasks.stop_transport().await;
    stopped.context("VM stop not confirmed")
}

/// Add the browser shim to `env` (see [`guest_env`]).
fn apply_browser_shim(env: &[String], user_browser: bool) -> Vec<String> {
    let bin_dir = BROWSER_SHIM.rsplit_once('/').map_or("/", |(dir, _)| dir);
    let path = env
        .iter()
        .find_map(|e| e.strip_prefix("PATH="))
        .filter(|p| !p.is_empty())
        .unwrap_or(oci::DEFAULT_PATH);
    let mut overrides = vec![("PATH", format!("{bin_dir}:{path}"))];
    if !user_browser {
        overrides.push(("BROWSER", BROWSER_SHIM.to_string()));
    }
    crate::util::merge_env(env, overrides, &[])
}

#[cfg(test)]
mod tests {
    //! Tests for the browser shim in the guest env.

    use super::*;

    /// The `entries` as owned strings.
    fn env(entries: &[&str]) -> Vec<String> {
        entries.iter().map(ToString::to_string).collect()
    }

    /// Test that the browser shim directory goes first on `PATH`, and that
    /// `BROWSER` points to the shim unless the user set `BROWSER`.
    ///   1. Apply the shim to an env without `BROWSER` and check `PATH` and
    ///      `BROWSER`
    ///   2. Apply the shim when the user set `BROWSER` and check that it stays
    ///   3. Apply the shim to an empty env and check that `PATH` uses the
    ///      default path
    #[test]
    fn browser_shim_goes_first_on_path_and_is_browser_unless_user_set_one() {
        assert_eq!(
            apply_browser_shim(&env(&["PATH=/usr/bin:/bin", "HOME=/root"]), false),
            [
                "HOME=/root",
                "PATH=/run/airlock/bin:/usr/bin:/bin",
                "BROWSER=/run/airlock/bin/xdg-open",
            ]
        );
        assert_eq!(
            apply_browser_shim(&env(&["PATH=/bin", "BROWSER=firefox"]), true),
            ["BROWSER=firefox", "PATH=/run/airlock/bin:/bin"]
        );
        assert_eq!(
            apply_browser_shim(&[], false)[0],
            format!("PATH=/run/airlock/bin:{}", oci::DEFAULT_PATH)
        );
    }
}
