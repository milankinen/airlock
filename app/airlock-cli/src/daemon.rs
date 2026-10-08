//! Daemon support on the host.
//!
//! Prepares the `[daemons.<name>]` sidecars for the sandbox:
//!  * converts the daemon config to the form that the guest supervisor uses
//!  * shows a summary of the daemons in verbose mode
//!  * shows the shutdown progress of each daemon after the main process exits
//!
//! The module keeps no state. The sandbox session uses it at the applicable
//! points of its lifecycle.

use std::collections::BTreeMap;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

use crate::config::config_values::RestartPolicy;
use crate::{cli, project, rpc};

/// Convert the enabled daemons of the project config to the wire format.
/// Args:
///  - `project`: Project with the daemon config and the vault
///  - `image_env`: Sandbox env (see [`crate::sandbox::boot::guest_env`]),
///    as `KEY=VALUE` items
///
/// Returns:
///   One [`rpc::DaemonSpec`] for each enabled daemon, or error if a
///   `${VAR}` template in a daemon env does not resolve. Daemon env values
///   override the sandbox env.
pub fn build_specs(
    project: &project::Project,
    image_env: &[String],
) -> anyhow::Result<Vec<rpc::DaemonSpec>> {
    project
        .config
        .daemons
        .iter()
        .filter(|(_, d)| d.enabled)
        .map(|(name, d)| {
            let overrides = d
                .env
                .iter()
                .map(|(key, template)| {
                    let value = project
                        .context
                        .vault
                        .subst(template)
                        .map_err(|e| anyhow::anyhow!("daemons.{name}.env.{key}: {e}"))?;
                    Ok((key, value))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let env = crate::util::merge_env(image_env, overrides, &[]);
            Ok(rpc::DaemonSpec {
                name: name.clone(),
                command: d.command.clone(),
                env,
                cwd: d.cwd.clone(),
                signal: d.signal.as_number(),
                timeout_ms: d.timeout.saturating_mul(1000),
                restart: d.restart,
                max_restarts: d.max_restarts,
                harden: d.harden,
            })
        })
        .collect()
}

/// Print a summary of the enabled daemons in verbose mode.
pub fn print_verbose(project: &project::Project) {
    let enabled: Vec<_> = project
        .config
        .daemons
        .iter()
        .filter(|(_, d)| d.enabled)
        .collect();
    if enabled.is_empty() {
        return;
    }
    cli::verbose!("  {} daemons: {}", cli::bullet(), enabled.len());
    for (name, d) in &enabled {
        let cmd = d.command.join(" ");
        let restart = match d.restart {
            RestartPolicy::Always => "always",
            RestartPolicy::OnFailure => "on-failure",
        };
        let max = if d.max_restarts == 0 {
            "\u{221e}".to_string()
        } else {
            d.max_restarts.to_string()
        };
        cli::verbose!(
            "      {name}: {cmd} (restart={restart}, max={max}, harden={})",
            d.harden
        );
    }
}

/// Stop all daemons and show one spinner per daemon until each daemon is in
/// a terminal state. Each spinner ends with "shut down", or "killed" if the
/// daemon got SIGKILL. Ctrl+C stops the wait. The caller's `vm.shutdown()`
/// then stops the VM and all daemons that still run.
/// Args:
///  - `supervisor`: Supervisor RPC client of the running VM
///  - `names`: Names of the daemons to wait for
pub async fn run_shutdown(supervisor: &rpc::Supervisor, names: &[String]) {
    supervisor.shutdown_daemons().await;

    let mp = cli::multi_progress();
    let style = ProgressStyle::with_template("{spinner} {msg}").unwrap();
    let mut bars: BTreeMap<String, ProgressBar> = BTreeMap::new();
    for name in names {
        let pb = mp.add(ProgressBar::new_spinner());
        pb.set_style(style.clone());
        pb.set_message(format!("daemon {name}: shutting down..."));
        pb.enable_steady_tick(Duration::from_millis(100));
        bars.insert(name.clone(), pb);
    }

    loop {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
            _ = tokio::signal::ctrl_c() => {
                for pb in bars.values() {
                    pb.finish_and_clear();
                }
                cli::error!("Killed by user");
                return;
            }
        }
        let states = match supervisor.poll_daemons().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("poll_daemons: {e}");
                for pb in bars.values() {
                    pb.finish_and_clear();
                }
                return;
            }
        };
        for (name, state) in &states {
            if !state.is_terminal() {
                continue;
            }
            let Some(pb) = bars.remove(name) else {
                continue;
            };
            pb.finish_and_clear();
            let label = match state {
                rpc::DaemonState::Killed => "killed",
                _ => "shut down",
            };
            let _ = mp.println(format!("{} daemon {name}: {label}", cli::check()));
        }
        if bars.is_empty() {
            break;
        }
    }
}
