//! The `airlock show` command.
//!
//! Prints the details of the project sandbox: status, config, packs and network
//! services.

use clap::Args;

use crate::config::ResolvedConfig;
use crate::context::Context;
use crate::packs::ConfiguredPack;
use crate::packs::install::state::{self, InstallState, PackStatus, ReadState};
use crate::services::{self, store};
use crate::util::PinnedDir;
use crate::vault::VaultStorageType;
use crate::{cli, config, packs, project};

/// CLI arguments for `airlock show`.
#[derive(Args, Debug)]
pub struct ShowArgs {}

/// Print sandbox details (path, status, image, config) to stdout.
/// Returns:
///   Process exit code: 0 on success, 2 on a config error, 1 on other errors.
pub async fn main(_args: &ShowArgs, context: Context) -> i32 {
    // Check config errors first, with the config error exit code (same as
    // `start`).
    let ResolvedConfig { values, packs, .. } = match resolve_config().await {
        Ok(resolved) => resolved,
        Err(e) => {
            cli::error!("Config error: {e:#}");
            return 2;
        }
    };
    let project = match project::load(values, context) {
        Ok(s) => s,
        Err(e) => {
            cli::error!("Sandbox details loading failed: {e:#}");
            return 1;
        }
    };

    if !project.sandbox_dir.exists() {
        cli::error!(
            "No sandbox for {} — run `airlock start` first",
            project.host_cwd.display()
        );
        return 1;
    }

    let status = if project.is_running() {
        cli::red("running")
    } else {
        cli::dim("stopped")
    };

    println!("Path:     {}", project.display_cwd());
    println!("Status:   {status}");
    println!("Image:    {}", project.config.vm.image);
    println!("CPUs:     {}", project.config.vm.cpus);
    println!("Memory:   {}", project.config.vm.memory);

    if let Some(ago) = project.last_run_ago() {
        println!("Last run: {ago}");
    }

    println!("Sandbox:  {}", project.sandbox_dir.display());

    if let Some((used, total)) = project.disk_usage() {
        println!(
            "Disk:     {} / {}",
            cli::format_bytes(used),
            cli::format_bytes(total)
        );
    }

    print_packs(&project, &packs);

    if !project.config.disk.cache.is_empty() {
        println!("Disk cache:");
        for (key, mount) in &project.config.disk.cache {
            let status = if mount.enabled { "" } else { " (disabled)" };
            println!("  {key}: {}{status}", mount.paths.join(", "));
        }
    }

    if !project.config.mounts.is_empty() {
        println!("Mounts:");
        for (key, mount) in &project.config.mounts {
            let status = if mount.enabled { "" } else { " (disabled)" };
            println!(
                "  {key}: {} \u{2192} {}{status}",
                mount.source, mount.target
            );
        }
    }

    println!("Network policy: {}", project.config.network.policy.label());

    if !project.config.network.rules.is_empty() {
        println!("Network rules:");
        for (key, rule) in &project.config.network.rules {
            let status = if rule.enabled { "" } else { " (disabled)" };
            println!(
                "  {key}: allow {} deny {}{status}",
                rule.allow.len(),
                rule.deny.len()
            );
        }
    }

    if !project.config.network.middleware.is_empty() {
        println!("Network middleware:");
        for (key, mw) in &project.config.network.middleware {
            let status = if mw.enabled { "" } else { " (disabled)" };
            println!("  {key}: {} targets{status}", mw.target.len());
        }
    }

    print_services(&project).await;

    0
}

/// Print the enabled network services, each with its stored sign-ins.
// Reads only the plain fields of the token store, so no vault key is needed.
async fn print_services(project: &project::Project) {
    let ids = services::enabled(&project.config.network.services);
    if ids.is_empty() {
        return;
    }
    println!("Services:");
    println!("  (sign-ins are the user's: all sandboxes share them)");
    let grants = if project.context.vault.storage_type() == VaultStorageType::Disabled {
        Err(anyhow::anyhow!("the vault is disabled"))
    } else {
        store::list_grants(&project.context.db)
            .await
            .map_err(|e| e.context("cannot read the token store"))
    };
    let grants = match grants {
        Ok(grants) => grants,
        Err(e) => {
            for id in ids {
                println!("  {}", unavailable_line(id, &format!("{e:#}")));
            }
            return;
        }
    };
    for id in ids {
        let lines: Vec<String> = grants
            .iter()
            .filter(|g| g.service == id.name())
            .map(grant_line)
            .collect();
        if lines.is_empty() {
            println!("  {}: not signed in", id.name());
            continue;
        }
        println!("  {}:", id.name());
        for line in lines {
            println!("    {line}");
        }
    }
}

/// Return the line for an enabled service that cannot run. The line gives
/// the reason and tells that the service hosts are denied.
fn unavailable_line(id: services::ServiceId, reason: &str) -> String {
    let name = id.name();
    format!(
        "{name}: unavailable ({reason}); its hosts are denied \
         (`[network.services] {name} = false` to sign in without airlock)"
    )
}

/// Return the line for one stored sign-in: account, scopes and age.
// The store does not record if the sign-in still works. The agent knows
// that, because the agent refreshes the sign-in itself.
fn grant_line(grant: &store::GrantSummary) -> String {
    let account = grant.account.as_deref().unwrap_or("unknown account");
    let scopes = match grant.scopes.as_slice() {
        [] => "no scopes".to_string(),
        [one] => one.clone(),
        many => format!("{} scopes", many.len()),
    };
    let age = std::time::Duration::from_millis(
        u64::try_from(store::now_ms() - grant.created_at).unwrap_or_default(),
    );
    format!(
        "{account} ({scopes}), saved {}",
        timeago::Formatter::new().convert(age)
    )
}

/// Load and resolve the config of the project in the current directory.
async fn resolve_config() -> anyhow::Result<ResolvedConfig> {
    let packs = packs::init()?;
    config::load()?
        .resolve(&packs, &config::ConfigOverrides::default())
        .await
}

/// Print the enabled packs with their status. Also print the install records
/// of packs that no longer install.
fn print_packs(project: &project::Project, packs: &[ConfiguredPack]) {
    let state = read_install_state(project);
    let removed = state
        .as_ref()
        .map(|s| removed_packs(packs, s))
        .unwrap_or_default();
    if packs.is_empty() && removed.is_empty() {
        return;
    }
    println!("Packs:");
    for pack in packs {
        println!(
            "  {} \u{2014} {}",
            pack_line(pack),
            pack_status(pack, state.as_ref())
        );
    }
    for (name, status) in removed {
        println!("  {name} \u{2014} {status}");
    }
}

/// Return the install records of packs that no longer install, with their
/// status.
///
/// A pack no longer installs if it was removed or disabled, or if its
/// selected version has no install.
fn removed_packs<'a>(
    packs: &[ConfiguredPack],
    state: &'a InstallState,
) -> Vec<(&'a str, &'static str)> {
    state
        .packs
        .iter()
        .filter(|(name, _)| {
            !packs
                .iter()
                .any(|p| p.metadata().name == name.as_str() && p.metadata().has_setup)
        })
        .filter_map(|(name, r)| removed_status(r.status).map(|st| (name.as_str(), st)))
        .collect()
}

/// Return the line for an enabled pack: `<name> <version>`, then the args
/// that are not default, in parentheses.
fn pack_line(pack: &ConfiguredPack) -> String {
    let details: Vec<String> = pack
        .non_default_args()
        .iter()
        .map(|(key, value)| format!("{key} = {value}"))
        .collect();
    let metadata = pack.metadata();
    let head = format!("{} {}", metadata.name, metadata.version);
    if details.is_empty() {
        head
    } else {
        format!("{head} ({})", details.join(", "))
    }
}

/// Return the status of an enabled pack. This is `config only` for a version
/// without a setup script. Otherwise it is the install status.
fn pack_status(pack: &ConfiguredPack, state: Option<&InstallState>) -> &'static str {
    if pack.metadata().has_setup {
        install_status(state, &pack.metadata().name)
    } else {
        "config only"
    }
}

/// Open `host_cwd/.airlock/sandbox` without the lock.
/// Returns:
///   `Ok(None)` if the directory does not exist yet (no sandbox started).
///   `Err` with a short message for all other failures (a symlink, a foreign
///   owner, ...), so that callers do not treat it as absent.
fn open_sandbox_dir(host_cwd: &std::path::Path) -> Result<Option<PinnedDir>, String> {
    match PinnedDir::open(host_cwd, std::path::Path::new(".airlock/sandbox"), false) {
        Ok(dir) => Ok(Some(dir)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Read the install records of the current project disk, without the lock.
/// Returns:
///   `None` if there are no records, or if they belong to another disk.
fn read_install_state(project: &project::Project) -> Option<InstallState> {
    let dir = match open_sandbox_dir(&project.host_cwd) {
        Ok(Some(dir)) => dir,
        Ok(None) => return None,
        Err(why) => {
            println!("  (cannot read .airlock/sandbox: {why})");
            return None;
        }
    };
    match state::read(&dir) {
        ReadState::Ok(s)
            if s.disk.is_some() && s.disk == project::disk_id(&project.sandbox_dir) =>
        {
            Some(s)
        }
        _ => None,
    }
}

/// Return the install status of the pack `name`.
fn install_status(state: Option<&InstallState>, name: &str) -> &'static str {
    match state.and_then(|s| s.packs.get(name)).map(|r| r.status) {
        Some(PackStatus::Installed | PackStatus::Kept { confirmed: true }) => "installed",
        Some(PackStatus::Unconfirmed | PackStatus::Kept { confirmed: false }) => {
            "install not confirmed"
        }
        Some(PackStatus::Failed) => "install failed",
        None => "pending",
    }
}

/// Return the status of a pack that no longer installs. Return `None` if no
/// part of it is on the disk.
fn removed_status(status: PackStatus) -> Option<&'static str> {
    match status {
        PackStatus::Installed | PackStatus::Unconfirmed => Some("installed, removed from config"),
        PackStatus::Kept { .. } => Some("kept, removed from config"),
        PackStatus::Failed => None,
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the parts of `airlock show`: the sandbox directory, the
    //! pack lines and the sign-in lines.

    use super::*;
    use crate::test_cfg::{resolve_project_toml, temp_dir};

    /// Test that a symlinked sandbox directory is an error, not a missing
    /// directory. Thus `show` reports the problem and does not tell the user
    /// that no sandbox exists.
    ///   1. Check that a project without `.airlock/sandbox` gives no directory
    ///   2. Make `.airlock/sandbox` a symlink to a different directory
    ///   3. Check that the open fails with a message
    #[test]
    fn sandbox_dir_behind_symlink_is_unreadable_not_missing() {
        let project = temp_dir();
        let elsewhere = temp_dir();
        assert!(matches!(open_sandbox_dir(project.path()), Ok(None)));

        std::fs::create_dir_all(project.path().join(".airlock")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), project.path().join(".airlock/sandbox"))
            .unwrap();

        assert!(open_sandbox_dir(project.path()).is_err_and(|why| !why.is_empty()));
    }

    /// Test that show gives each pack the status from the install records, and
    /// lists the installed packs that the config no longer has.
    ///   1. Make install records with different statuses
    ///   2. Resolve three packs and check their lines and statuses
    ///   3. Check the status of a pack when there are no records
    ///   4. Check the removed packs for the pack config and for a presets list
    #[test]
    fn pack_status_reflects_install_records_of_sandbox_disk() {
        let fp = "a".repeat(64);
        let mut state = InstallState::default();
        state.set("rust", PackStatus::Installed, &fp);
        state.set("sample", PackStatus::Kept { confirmed: false }, &fp);
        state.set("python", PackStatus::Failed, &fp);
        state.set("node", PackStatus::Unconfirmed, &fp);
        state.set("go", PackStatus::Kept { confirmed: true }, &fp);

        let resolved = resolve_project_toml(
            "[packs]\nrust = { version = 1 }\n\
             sample = { version = 1, args = { mode = \"slow\", network = true } }\n\
             alpine = { version = 1 }\n",
        )
        .unwrap();
        // alpine has no setup script, so it is config only. sample was
        // kept but not confirmed.
        let shown: Vec<(String, &str)> = resolved
            .packs
            .iter()
            .map(|p| (pack_line(p), pack_status(p, Some(&state))))
            .collect();
        assert_eq!(
            shown,
            [
                ("alpine 1".to_string(), "config only"),
                ("rust 1".to_string(), "installed"),
                (
                    "sample 1 (mode = slow)".to_string(),
                    "install not confirmed"
                ),
            ]
        );
        assert_eq!(pack_status(&resolved.packs[1], None), "pending");
        // Failed records are not shown. Records of packs in the config are
        // not removed.
        assert_eq!(
            removed_packs(&resolved.packs, &state),
            [
                ("go", "kept, removed from config"),
                ("node", "installed, removed from config"),
            ]
        );

        // A presets list makes no packs, so all records count as removed.
        let list_form = resolve_project_toml("presets = [\"rust\", \"python\"]\n").unwrap();
        assert!(list_form.packs.is_empty());
        assert_eq!(
            removed_packs(&list_form.packs, &state),
            [
                ("go", "kept, removed from config"),
                ("node", "installed, removed from config"),
                ("rust", "installed, removed from config"),
                ("sample", "kept, removed from config"),
            ]
        );
    }

    /// Test that the line for a stored sign-in shows the account, the scopes
    /// and the age.
    ///   1. Check the line for one scope and for two scopes
    ///   2. Check the line for a sign-in without an account or scopes
    #[test]
    fn stored_sign_in_shows_account_scopes_and_age() {
        let grant = |scopes: &[&str]| store::GrantSummary {
            service: "anthropic".into(),
            account: Some("a@example.com".into()),
            scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
            created_at: store::now_ms() - 3 * 24 * 60 * 60 * 1000,
        };
        assert_eq!(
            grant_line(&grant(&["user:inference"])),
            "a@example.com (user:inference), saved 3 days ago"
        );
        assert_eq!(
            grant_line(&grant(&["a", "b"])),
            "a@example.com (2 scopes), saved 3 days ago"
        );
        let anonymous = store::GrantSummary {
            account: None,
            ..grant(&[])
        };
        assert_eq!(
            grant_line(&anonymous),
            "unknown account (no scopes), saved 3 days ago"
        );
    }
}
