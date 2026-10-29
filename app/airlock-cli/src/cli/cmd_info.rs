//! The `airlock info` command (alias `show`).
//!
//! Prints the details of a sandbox: status, disk, config, packs and network
//! services. Without an id, it is the sandbox of the current directory.
//! `airlock sandboxes info` gives the same output for any sandbox.

use std::collections::BTreeMap;
use std::ffi::OsString;

use clap::Args;
use serde::Serialize;

use crate::config::ResolvedConfig;
use crate::context::Context;
use crate::packs::ConfiguredPack;
use crate::packs::install::state::{self, InstallState, PackStatus, ReadState};
use crate::project::read_run_meta;
use crate::sandboxes::{Found, registry};
use crate::services::{self, store};
use crate::util::PinnedDir;
use crate::util::verified_path::VerifiedPath;
use crate::vault::VaultStorageType;
use crate::vm::disk;
use crate::{cli, config, messages, packs, project, sandboxes};

/// CLI arguments for `airlock info`.
#[derive(Args, Debug)]
pub struct InfoArgs {
    /// Print the sandbox details as JSON
    #[arg(long)]
    pub json: bool,
}

/// Print the details of the sandbox of the current directory (see [`run`]).
/// Returns:
///   Process exit code: 0 on success, 2 on a config error, 1 on other errors.
pub async fn main(args: &InfoArgs, context: Context) -> i32 {
    run(context, None, args.json).await
}

/// Print the details of the sandbox `id`, or of the sandbox of the current
/// directory. The text has the sandbox and the config of its project. JSON
/// has only the sandbox. A sandbox whose project directory is gone shows
/// only the sandbox.
/// Returns:
///   Process exit code: 0 on success, 2 on a config error, 1 on other errors.
pub(super) async fn run(context: Context, id: Option<&str>, json: bool) -> i32 {
    // A registered project path that fails the check (for example a symlink
    // in it) shows as text, like in `sandboxes ls`.
    if let Some(id) = id
        && let Ok(project) = registered_project(&context, id).await
        && VerifiedPath::new(&project, &context.roots()).is_err()
        && let Ok(dir) = context.data.sandboxes.join(id)
    {
        let details = Details::read(id, project.display().to_string(), &dir);
        return print_details_only(&details, json);
    }
    let (project_dir, found) = match find(&context, id).await {
        Ok(found) => found,
        Err(e) => {
            cli::error!("{e:#}");
            return 1;
        }
    };
    // Check config errors first, with the config error exit code (same as
    // `start`). The local project config is in the sandbox directory. A
    // project with no sandbox uses `.airlock` in the project.
    let resolved = if project_dir.is_dir() {
        let local_dir = match &found {
            None => project_dir.join(".airlock"),
            Some(found) => sandboxes::local_config_dir(&found.dir),
        };
        let local_dir = match local_dir {
            Ok(dir) => dir,
            Err(e) => {
                cli::error!("{e}");
                return 1;
            }
        };
        match resolve_config(&context, &project_dir, &local_dir).await {
            Ok(resolved) => Some(resolved),
            Err(e) => {
                cli::error!("{}", messages::cli::config_error(format!("{e:#}")));
                return 2;
            }
        }
    } else {
        None
    };
    let found = match found {
        Some(found) if found.dir.symlink_metadata().is_ok() => found,
        _ => {
            cli::error!("{}", messages::cli::no_sandbox_for_project(&project_dir));
            return 1;
        }
    };
    let details = Details::read(&found.id, found.project.to_string(), &found.dir);
    if json {
        return print_json(&details);
    }
    let Some(ResolvedConfig { values, packs, .. }) = resolved else {
        print_sandbox_only(&details);
        return 0;
    };
    let project = project::load(values, context, &found);

    let status = if project.is_running() {
        cli::red(messages::cli::STATUS_RUNNING)
    } else {
        cli::dim(messages::cli::STATUS_STOPPED)
    };

    println!("{}", messages::cli::info_path(project.display_cwd()));
    println!("{}", messages::cli::info_id(&details.id));
    println!("{}", messages::cli::info_status(status));
    println!("{}", messages::cli::info_image(&project.config.vm.image));
    println!("{}", messages::cli::info_cpus(project.config.vm.cpus));
    println!("{}", messages::cli::info_memory(project.config.vm.memory));

    if let Some(ago) = project.last_run_ago() {
        println!("{}", messages::cli::info_last_run(ago));
    }

    println!("{}", messages::cli::info_sandbox(&project.sandbox_dir));

    if let Some(disk) = details.disk_text() {
        println!("{}", messages::cli::info_disk(disk));
    }

    print_packs(&project, &packs);

    if !project.config.disk.cache.is_empty() {
        println!("{}", messages::cli::INFO_DISK_CACHE);
        for (key, mount) in &project.config.disk.cache {
            let status = if mount.enabled {
                ""
            } else {
                messages::cli::DISABLED_SUFFIX
            };
            println!(
                "  {}",
                messages::cli::info_disk_cache_entry(key, mount.paths.join(", "), status)
            );
        }
    }

    if !project.config.mounts.is_empty() {
        println!("{}", messages::cli::INFO_MOUNTS);
        for (key, mount) in &project.config.mounts {
            let status = if mount.enabled {
                ""
            } else {
                messages::cli::DISABLED_SUFFIX
            };
            println!(
                "  {}",
                messages::cli::info_mount(key, &mount.source, &mount.target, status)
            );
        }
    }

    println!(
        "{}",
        messages::cli::info_network_policy(project.config.network.policy.label())
    );

    if !project.config.network.rules.is_empty() {
        println!("{}", messages::cli::INFO_NETWORK_RULES);
        for (key, rule) in &project.config.network.rules {
            let status = if rule.enabled {
                ""
            } else {
                messages::cli::DISABLED_SUFFIX
            };
            println!(
                "  {}",
                messages::cli::info_network_rule(key, rule.allow.len(), rule.deny.len(), status)
            );
        }
    }

    if !project.config.network.middleware.is_empty() {
        println!("{}", messages::cli::INFO_NETWORK_MIDDLEWARE);
        for (key, mw) in &project.config.network.middleware {
            let status = if mw.enabled {
                ""
            } else {
                messages::cli::DISABLED_SUFFIX
            };
            println!(
                "  {}",
                messages::cli::info_network_middleware(key, mw.target.len(), status)
            );
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
    println!("{}", messages::cli::INFO_SERVICES);
    println!("  {}", messages::cli::SERVICES_SIGN_INS_SHARED);
    let grants = if project.context.vault.storage_type() == VaultStorageType::Disabled {
        Err(anyhow::anyhow!(messages::services::VAULT_DISABLED))
    } else {
        store::list_grants(&project.context.db)
            .await
            .map_err(|e| e.context(messages::cli::CANNOT_READ_TOKEN_STORE))
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
            println!("  {}", messages::cli::service_not_signed_in(id.name()));
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
    messages::cli::service_unavailable(name, reason)
}

/// Return the line for one stored sign-in: account, scopes and age.
// The store does not record if the sign-in still works. The agent knows
// that, because the agent refreshes the sign-in itself.
fn grant_line(grant: &store::GrantSummary) -> String {
    let account = grant
        .account
        .as_deref()
        .unwrap_or(messages::cli::UNKNOWN_ACCOUNT);
    let scopes = match grant.scopes.as_slice() {
        [] => messages::cli::NO_SCOPES.to_string(),
        [one] => one.clone(),
        many => messages::cli::scope_count(many.len()),
    };
    let age = std::time::Duration::from_millis(
        u64::try_from(store::now_ms() - grant.created_at).unwrap_or_default(),
    );
    messages::cli::grant_summary(account, scopes, timeago::Formatter::new().convert(age))
}

/// Load and resolve the config of a project.
/// Args:
///  - `context`: Process context: the data directory, and the settings and
///    home directory for the user packs
///  - `project_dir`: Project directory
///  - `local_dir`: Directory of the local project config
async fn resolve_config(
    context: &Context,
    project_dir: &VerifiedPath,
    local_dir: &VerifiedPath,
) -> anyhow::Result<ResolvedConfig> {
    let packs = packs::init(&context.settings.packs, &context.home)?;
    config::load(project_dir, local_dir)?
        .resolve(&config::ResolveInput {
            data_dir: &context.data,
            packs: &packs,
            overrides: &config::ConfigOverrides::default(),
            project_root: Some(project_dir),
            // Only `airlock start` asks about new or changed packs.
            approval: crate::packs::approval::ApprovalMode::Refuse,
        })
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
    println!("{}", messages::cli::INFO_PACKS);
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
                .any(|p| p.metadata().id == name.as_str() && p.metadata().has_setup)
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
    let head = metadata.id_with_version();
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
        install_status(state, pack.metadata().id.as_str()).text()
    } else {
        messages::cli::PACK_CONFIG_ONLY
    }
}

/// Open the sandbox directory `dir` without the lock.
/// Returns:
///   `Ok(None)` if the directory does not exist. `Err` with a short message
///   for all other failures (a symlink, a foreign owner, ...), so that
///   callers do not treat it as absent.
fn open_sandbox_dir(dir: &VerifiedPath) -> Result<Option<PinnedDir>, String> {
    match dir.to_path().and_then(PinnedDir::pin) {
        Ok(dir) => Ok(Some(dir)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Read the install records of the current project disk, without the lock.
/// Returns:
///   `None` if there are no records, or if they belong to another disk.
fn read_install_state(project: &project::Project) -> Option<InstallState> {
    let dir = match open_sandbox_dir(&project.sandbox_dir) {
        Ok(Some(dir)) => dir,
        Ok(None) => return None,
        Err(why) => {
            println!("  {}", messages::cli::cannot_read_sandbox_dir(why));
            return None;
        }
    };
    match state::read(&dir) {
        ReadState::Ok(s) if s.disk.is_some() && s.disk == project::disk_id(&dir) => Some(s),
        _ => None,
    }
}

/// Install status of a pack. The JSON output uses the serialized values.
/// They are machine output and do not change with the texts.
#[derive(Debug, Clone, Copy, Serialize)]
pub(super) enum InstallStatus {
    #[serde(rename = "installed")]
    Installed,
    #[serde(rename = "install not confirmed")]
    NotConfirmed,
    #[serde(rename = "install failed")]
    Failed,
    #[serde(rename = "pending")]
    Pending,
}

impl InstallStatus {
    /// Return the text of the status for the human output.
    fn text(self) -> &'static str {
        match self {
            Self::Installed => messages::cli::PACK_INSTALLED,
            Self::NotConfirmed => messages::cli::PACK_INSTALL_NOT_CONFIRMED,
            Self::Failed => messages::cli::PACK_INSTALL_FAILED,
            Self::Pending => messages::cli::PACK_PENDING,
        }
    }
}

/// Status of a sandbox. The JSON output uses the serialized values. They are
/// machine output and do not change with the texts.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum SandboxStatus {
    Running,
    Stopped,
    /// The sandbox has no directory.
    Missing,
}

impl SandboxStatus {
    /// Return the text of the status for the human output.
    pub(super) fn text(self) -> &'static str {
        match self {
            Self::Running => messages::cli::STATUS_RUNNING,
            Self::Stopped => messages::cli::STATUS_STOPPED,
            Self::Missing => messages::cli::STATUS_MISSING,
        }
    }
}

/// Return the install status of the pack `name`.
fn install_status(state: Option<&InstallState>, name: &str) -> InstallStatus {
    match state.and_then(|s| s.packs.get(name)).map(|r| r.status) {
        Some(PackStatus::Installed | PackStatus::Kept { confirmed: true }) => {
            InstallStatus::Installed
        }
        Some(PackStatus::Unconfirmed | PackStatus::Kept { confirmed: false }) => {
            InstallStatus::NotConfirmed
        }
        Some(PackStatus::Failed) => InstallStatus::Failed,
        None => InstallStatus::Pending,
    }
}

/// Return the status of a pack that no longer installs. Return `None` if no
/// part of it is on the disk.
fn removed_status(status: PackStatus) -> Option<&'static str> {
    match status {
        PackStatus::Installed | PackStatus::Unconfirmed => {
            Some(messages::cli::PACK_INSTALLED_REMOVED)
        }
        PackStatus::Kept { .. } => Some(messages::cli::PACK_KEPT_REMOVED),
        PackStatus::Failed => None,
    }
}

/// Find the sandbox `id`, or the sandbox of the current directory.
/// Returns:
///   The project directory and its sandbox (`None` if the current directory
///   has none), or error for an id that is not registered.
pub(super) async fn find(
    context: &Context,
    id: Option<&str>,
) -> anyhow::Result<(VerifiedPath, Option<Found>)> {
    if let Some(id) = id {
        let found = find_by_id(context, id).await?;
        return Ok((found.project.clone(), Some(found)));
    }
    let cwd = sandboxes::project_dir(&context.work)?;
    let found = sandboxes::resolve_sandbox(context, &cwd, false, false)
        .await
        .map_err(|e| e.context(messages::cli::SANDBOX_LOOKUP_FAILED_CONTEXT))?;
    Ok((cwd, found))
}

/// Get the registered sandbox `id`.
/// Returns:
///   The sandbox, or error if `id` is not registered or a part of its
///   project path before the last is a symlink.
async fn find_by_id(context: &Context, id: &str) -> anyhow::Result<Found> {
    let project = registered_project(context, id).await?;
    Ok(Found {
        id: id.to_string(),
        project: VerifiedPath::new(project, &context.roots())?,
        dir: context.data.sandboxes.join(id)?,
    })
}

/// Get the project directory of the registered sandbox `id`, as the
/// registry has it.
/// Returns:
///   The project directory, or error if `id` is not registered.
pub(super) async fn registered_project(context: &Context, id: &str) -> anyhow::Result<OsString> {
    let project = if registry::is_valid_id(id) {
        registry::get(&context.db, id).await?
    } else {
        None
    };
    project.ok_or_else(|| anyhow::anyhow!(messages::cli::no_sandbox_with_id(id)))
}

/// Details of one sandbox, without its config. The JSON output and the
/// sandbox list use them.
#[derive(Debug, Serialize)]
pub(super) struct Details {
    /// Registry id.
    pub(super) id: String,
    /// Always `data-dir`. Older versions also had `project-dir`.
    location: &'static str,
    /// Project directory.
    pub(super) project: String,
    /// Sandbox directory.
    dir: String,
    /// `running`, `stopped` or `missing` (no sandbox directory).
    pub(super) status: SandboxStatus,
    /// Unix time of the last boot.
    pub(super) last_run: Option<u64>,
    /// Working directory in the guest of the last run.
    guest_cwd: Option<String>,
    /// Allocated size of the disk image in bytes.
    pub(super) disk_used: Option<u64>,
    /// Size of the disk image in bytes.
    disk_size: Option<u64>,
    /// Install status of each pack with a record.
    packs: BTreeMap<String, InstallStatus>,
}

impl Details {
    /// Read the details of a sandbox. Takes no lock and writes nothing.
    /// Records that cannot be read are left out.
    /// Args:
    ///  - `id`: Registry id of the sandbox
    ///  - `project`: Project directory, for the output only
    ///  - `dir`: Sandbox directory
    pub(super) fn read(id: &str, project: String, dir: &VerifiedPath) -> Self {
        use std::os::unix::fs::MetadataExt;
        let status = if dir.symlink_metadata().is_err() {
            SandboxStatus::Missing
        } else if project::is_running(dir) {
            SandboxStatus::Running
        } else {
            SandboxStatus::Stopped
        };
        let run = read_run_meta(dir);
        let mut packs = BTreeMap::new();
        // A symlinked sandbox directory is not read.
        if let Ok(pinned) = dir.to_path().and_then(PinnedDir::pin)
            && let ReadState::Ok(installs) = state::read(&pinned)
        {
            for name in installs.packs.keys() {
                packs.insert(name.clone(), install_status(Some(&installs), name));
            }
        }
        // The disk is a sparse file: `blocks() * 512` gives the allocated
        // size.
        let disk = dir.join(disk::DISK_FILE).and_then(|p| p.metadata()).ok();
        Self {
            id: id.to_string(),
            location: "data-dir",
            project,
            dir: dir.to_string(),
            status,
            last_run: run.last_run,
            guest_cwd: run.guest_cwd,
            disk_used: disk.as_ref().map(|m| m.blocks() * 512),
            disk_size: disk.as_ref().map(std::fs::Metadata::len),
            packs,
        }
    }

    /// Format the disk use as `<used> / <size>`.
    fn disk_text(&self) -> Option<String> {
        Some(messages::cli::disk_use(
            cli::format_bytes(self.disk_used?),
            cli::format_bytes(self.disk_size?),
        ))
    }
}

/// Print the details as JSON.
/// Returns:
///   Process exit code.
fn print_json(details: &Details) -> i32 {
    match serde_json::to_string_pretty(details) {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(e) => {
            cli::error!("{e}");
            1
        }
    }
}

/// Print only the sandbox details, as JSON or as text.
/// Returns:
///   Process exit code.
fn print_details_only(details: &Details, json: bool) -> i32 {
    if json {
        return print_json(details);
    }
    print_sandbox_only(details);
    0
}

/// Print the details of a sandbox whose project directory is gone. There is
/// no config to show.
fn print_sandbox_only(details: &Details) {
    println!("{}", messages::cli::info_path_missing(&details.project));
    println!("{}", messages::cli::info_id(&details.id));
    println!("{}", messages::cli::info_status(details.status.text()));
    if let Some(last_run) = details.last_run {
        println!(
            "{}",
            messages::cli::info_last_run(project::time_ago(last_run))
        );
    }
    println!("{}", messages::cli::info_sandbox(&details.dir));
    if let Some(disk) = details.disk_text() {
        println!("{}", messages::cli::info_disk(disk));
    }
    if !details.packs.is_empty() {
        println!("{}", messages::cli::INFO_PACKS);
        for (name, status) in &details.packs {
            println!("  {name} \u{2014} {}", status.text());
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the parts of `airlock info`: the sandbox directory, the
    //! pack lines and the sign-in lines.

    use super::*;
    use crate::test_cfg::{resolve_project_toml, temp_dir};

    /// Test that a symlinked sandbox directory is an error, not a missing
    /// directory. Thus `info` reports the problem and does not tell the user
    /// that no sandbox exists.
    ///   1. Check that a missing sandbox directory gives no directory
    ///   2. Make the sandbox directory a symlink to a different directory
    ///   3. Check that the open fails with a message
    #[test]
    fn sandbox_dir_behind_symlink_is_unreadable_not_missing() {
        let project = temp_dir();
        let elsewhere = temp_dir();
        let sandbox = VerifiedPath::new_follow(project.path(), None)
            .and_then(|p| p.join("data/boxes/0123456789abcdef"))
            .unwrap();
        assert!(matches!(open_sandbox_dir(&sandbox), Ok(None)));

        let boxes = project.path().join("data/boxes");
        std::fs::create_dir_all(&boxes).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), boxes.join("0123456789abcdef")).unwrap();

        assert!(open_sandbox_dir(&sandbox).is_err_and(|why| !why.is_empty()));
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
