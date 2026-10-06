//! Boot: the VM up with every capability the guest gets, supervisor and
//! network RPC served, the guest booted — and no process started.

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

/// How often the host wall-clock is pushed into the guest.
const CLOCK_SYNC_INTERVAL: Duration = Duration::from_mins(1);

/// How one boot presents itself and what it shares.
pub struct BootOptions {
    /// Guest kernel verbosity and the guest `tracing` filter.
    pub log_level: LogLevel,
    /// Skip the boot progress lines (the verbose config report, "Booting
    /// VM..." and the VM resources summary), for callers that print their
    /// own progress.
    pub quiet: bool,
    /// Mount the project directory and start processes in it. When false,
    /// the project is not shared and the guest works in `/`.
    pub project_share: bool,
}

/// Everything one boot gives the guest. The caller builds it; [`boot`]
/// only wires it into the VM.
pub struct BootSpec<'a> {
    pub project: Project,
    pub image: &'a OciImage,
    pub options: BootOptions,
    /// The sandbox env of every process and daemon (see [`guest_env`]).
    pub env: Vec<String>,
    pub network: Network,
    /// The browser bridge; `None` grants no browser.
    pub browser: Option<Browser>,
    /// The clipboard bridge; `None` grants no clipboard.
    pub clipboard: Option<ClipboardImpl>,
    pub daemons: Vec<rpc::DaemonSpec>,
    pub masks: Vec<rpc::MaskSpec>,
}

/// The sandbox env: the image env with the guest-visible `[env]` values
/// layered on top (masked entries carry their surrogate, never the real
/// value). With `browser_shim` (the boot grants a browser), the guest's
/// browser shim directory leads `PATH` (tools that run `xdg-open` find the
/// shim), and `BROWSER` names the shim unless the user's `[env]` sets
/// `BROWSER`.
pub fn guest_env(project: &Project, image: &OciImage, browser_shim: bool) -> Vec<String> {
    let env = crate::util::merge_env(&image.env, project.env.guest_entries(), &[]);
    if !browser_shim {
        return env;
    }
    let user_browser = project.config.env.contains_key("BROWSER");
    apply_browser_shim(&env, user_browser)
}

/// Boot a VM for `spec.project` with `spec.image`: bind the reverse port
/// forwards, start the VM, serve the supervisor and network RPC, and boot
/// the guest (`Supervisor.boot`: mounts, networking, daemons, and the
/// grants of `spec`). Starts no process. Fails with [`super::Interrupted`]
/// when the user interrupts before or during the boot. Any failure after
/// the VM started stops it again first.
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

    // Check if user interrupted during setup (e.g. Ctrl+C during download)
    if cli::is_interrupted() {
        return Err(super::Interrupted.into());
    }

    // Bind reverse port forward listeners before booting the VM so that
    // bind errors (e.g. EADDRINUSE) surface immediately, without the VM
    // boot output in the way. The listeners are held until the supervisor
    // is ready, at which point accept loops are spawned against them.
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
        // No daemon runs after a failed boot: shut down without them.
        let vm = Vm::new(project, instance, supervisor, network, vec![], tasks);
        return match vm.shutdown().await {
            Ok(_) => Err(e),
            Err(stop) => Err(stop.context(format!("boot failed: {e:#}"))),
        };
    }
    info!("vm booted");
    // Push the host wall-clock into the guest every minute so the VM clock
    // stays in sync across host sleeps (laptop lid closed, suspend). VMs
    // have no RTC, so without this the guest time drifts by exactly the
    // sleep duration.
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

/// Connect the RPC channels of a started VM. The supervisor connection
/// comes first; the guest accepts the network channel right after it.
async fn connect(
    vm: &VmInstance,
    vsock_fd: OwnedFd,
    network: Network,
    reverse_forwards: Vec<BoundForward>,
    tasks: &mut BootTasks,
) -> anyhow::Result<(rpc::Supervisor, NetworkHandle, Vec<(String, String)>)> {
    // A Ctrl+C during boot (the vsock connect can retry for ~12s) sets the
    // interrupt flag, but the boot path doesn't watch it. Catch it here —
    // before we invest in supervisor setup — and tear the freshly-booted
    // VM back down instead of booting a sandbox the user already
    // cancelled.
    if cli::is_interrupted() {
        info!("interrupted during boot; shutting down VM");
        return Err(super::Interrupted.into());
    }

    let (supervisor, driver) = rpc::Supervisor::connect(vsock_fd)?;
    tasks.spawn_transport(driver);
    tasks.spawn_service(network.deny_reporter().attach(supervisor.client()));

    // Wire the pre-bound reverse port forward listeners into the now-ready
    // supervisor.
    let guest = GuestNetwork::new(supervisor.client());
    network::reverse_forward::serve(reverse_forwards, &guest, tasks.services());

    // Extract socket-forward metadata and the handle before `Network` is
    // consumed by the NetworkProxy RPC server.
    let socket_fwds = network
        .socket_map
        .iter()
        .map(|(guest, host)| (host.to_string_lossy().into_owned(), guest.clone()))
        .collect();
    let handle = network.handle();

    // Open the dedicated vsock for NetworkProxy RPC and serve `Network`
    // as its bootstrap capability. Keeps bulk byte relays off the
    // supervisor channel so pty / stats / daemon traffic can't be
    // head-of-line-blocked.
    let network_fd = vm.vsock_connect(airlock_common::NETWORK_PORT).await?;
    tasks.spawn_transport(rpc::serve_network(network_fd, network)?);

    Ok((supervisor, handle, socket_fwds))
}

/// Tear down a boot that has no usable supervisor: services, then the VM
/// (confirmed), then the transport.
async fn stop_vm(vm: VmInstance, mut tasks: BootTasks) -> anyhow::Result<()> {
    tasks.stop_services().await;
    info!("vm shutdown");
    let stopped = vm.shutdown().await;
    tasks.stop_transport().await;
    stopped.context("VM stop not confirmed")
}

/// `env` with the browser shim (see [`guest_env`]).
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
    use super::*;

    fn env(entries: &[&str]) -> Vec<String> {
        entries.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_browser_grant_points_the_env_at_the_shim() {
        let got = apply_browser_shim(&env(&["PATH=/usr/bin:/bin", "HOME=/root"]), false);
        assert_eq!(
            got,
            env(&[
                "HOME=/root",
                "PATH=/run/airlock/bin:/usr/bin:/bin",
                "BROWSER=/run/airlock/bin/xdg-open",
            ])
        );
    }

    #[test]
    fn a_browser_of_the_user_stays() {
        let got = apply_browser_shim(&env(&["PATH=/bin", "BROWSER=firefox"]), true);
        assert_eq!(got, env(&["BROWSER=firefox", "PATH=/run/airlock/bin:/bin"]));
    }

    #[test]
    fn no_path_gets_the_default() {
        let got = apply_browser_shim(&[], false);
        assert_eq!(
            got[0],
            format!("PATH=/run/airlock/bin:{}", oci::DEFAULT_PATH)
        );
    }
}
