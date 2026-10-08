//! `airlock show` — display sandbox details.

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

/// Print sandbox metadata (path, status, image, config) to stdout.
pub async fn main(_args: &ShowArgs, context: Context) -> i32 {
    // Config errors first, with the config-error exit code (as `start`).
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

/// The enabled network services, each with its stored sign-ins. Reads
/// only the plain fields of the token store: no vault key needed.
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

/// An enabled service that cannot run: why, and that its hosts are
/// denied.
fn unavailable_line(id: services::ServiceId, reason: &str) -> String {
    let name = id.name();
    format!(
        "{name}: unavailable ({reason}); its hosts are denied \
         (`[network.services] {name} = false` to sign in without airlock)"
    )
}

/// One stored sign-in: account, scopes and age. Whether it still works is
/// the agent's to find out (it refreshes itself); the store does not
/// track that.
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

/// The config of the project in the current directory.
async fn resolve_config() -> anyhow::Result<ResolvedConfig> {
    let packs = packs::init()?;
    config::load()?
        .resolve(&packs, &config::ConfigOverrides::default())
        .await
}

/// The enabled packs with their status, and the install records of
/// packs that no longer install.
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

/// The install records of packs whose selected entry no longer
/// installs (removed, disabled, or a version without an install), with
/// their status.
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

/// An enabled pack: `<name> <version>`, then the args that differ from
/// their default in parentheses.
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

/// The status of an enabled pack: `config only` for a version without
/// a setup script, else the install status.
fn pack_status(pack: &ConfiguredPack, state: Option<&InstallState>) -> &'static str {
    if pack.metadata().has_setup {
        install_status(state, &pack.metadata().name)
    } else {
        "config only"
    }
}

/// Open `host_cwd/.airlock/sandbox` without the lock. `Ok(None)` when it
/// does not exist yet (no sandbox has started); `Err` with a short message
/// for any other failure (a symlink, a foreign owner, ...), so callers do
/// not silently treat it as absent.
fn open_sandbox_dir(host_cwd: &std::path::Path) -> Result<Option<PinnedDir>, String> {
    match PinnedDir::open(host_cwd, std::path::Path::new(".airlock/sandbox"), false) {
        Ok(dir) => Ok(Some(dir)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// The install records of the project's current disk, read without the
/// lock. `None` when there are none, or they belong to another disk.
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

/// The install status of the pack `name`.
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

/// The status of a pack that no longer installs; `None` when nothing of
/// it is on the disk.
fn removed_status(status: PackStatus) -> Option<&'static str> {
    match status {
        PackStatus::Installed | PackStatus::Unconfirmed => Some("installed, removed from config"),
        PackStatus::Kept { .. } => Some("kept, removed from config"),
        PackStatus::Failed => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_cfg::{resolve_project_toml, temp_dir};

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
        assert_eq!(
            removed_packs(&resolved.packs, &state),
            [
                ("go", "kept, removed from config"),
                ("node", "installed, removed from config"),
            ]
        );

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
