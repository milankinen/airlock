//! Host-side helpers for `[daemons.<name>]` sidecars.
//!
//! Two responsibilities:
//!   1. Translate the config-level `Daemon` map into wire-format `DaemonSpec`s
//!      (env expansion through the project vault, image env merge, filtering
//!      out disabled entries).
//!   2. Drive the post-main-shell shutdown UI: ask the supervisor to stop all
//!      daemons, then poll until each reports a terminal state.
//!
//! Neither helper owns any state — they're called from `cmd_start` at the
//! specific lifecycle points they apply to.

use std::collections::BTreeMap;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

use crate::config::config::RestartPolicy;
use crate::{cli, project, rpc};

/// Expand `${VAR}` templates in each daemon's env map and layer them on
/// top of the image env. Disabled daemons are filtered out.
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
            let mut env = image_env.to_vec();
            for (key, template) in &d.env {
                let value = project
                    .vault
                    .subst(template)
                    .map_err(|e| anyhow::anyhow!("daemons.{name}.env.{key}: {e}"))?;
                env.retain(|existing| !existing.starts_with(&format!("{key}=")));
                env.push(format!("{key}={value}"));
            }
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

/// Verbose-only summary of declared daemons. Disabled daemons are filtered out.
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

/// Ask the supervisor to stop every daemon, then drive one spinner per
/// daemon until all report a terminal state. Each spinner finishes with
/// either "shut down" or "killed" (for SIGKILL'd daemons). Ctrl+C
/// shortcircuits — the caller's `vm.shutdown()` will tear the VM down
/// and with it any still-running daemons.
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
