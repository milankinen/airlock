//! Run record of a sandbox.
//!
//! Keeps the data of the last start of a sandbox: its time, the guest
//! directory, the final config and the resolved host paths of the config.

use std::io::Write as _;
use std::time::{Duration, SystemTime};

use crate::util::VerifiedPath;

/// Size limit of `run.json`. Defense in depth: only airlock writes the
/// file.
const RUN_META_CAP: u64 = 4 * 1024 * 1024;

/// Format the last run time of the sandbox as "X ago".
/// Returns:
///   Formatted time, or `None` if `run.json` has no last run.
pub fn last_run_ago(sandbox_dir: &VerifiedPath) -> Option<String> {
    read_run_meta(sandbox_dir).last_run.map(time_ago)
}

/// Format the time since the Unix time `epoch` as "X ago".
pub fn time_ago(epoch: u64) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let elapsed = Duration::from_secs(now.saturating_sub(epoch));
    timeago::Formatter::new().convert(elapsed)
}

/// Run metadata stored in `run.json`.
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct RunMeta {
    /// Unix time of the last boot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<u64>,
    /// Working directory in the guest of the last run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_cwd: Option<String>,
    /// Final config of the last run (see [`crate::sandbox::start::config_change`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_config: Option<serde_json::Value>,
    /// Resolved host paths of the last run, by config key (see
    /// [`crate::sandbox::start::config_change`]). `None` in a file of an older
    /// airlock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_paths: Option<HostPaths>,
}

/// Host paths of a config, by config key, for example
/// `mounts.data.source`.
pub type HostPaths = std::collections::BTreeMap<String, HostPath>;

/// A host path of the config.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HostPath {
    /// The path as the config gives it, for example `./data`.
    pub given: String,
    /// The absolute path with its symlinks resolved.
    pub resolved: String,
}

/// Read `run.json`. Gives default metadata if the file is missing or not
/// valid.
pub fn read_run_meta(sandbox_dir: &VerifiedPath) -> RunMeta {
    sandbox_dir
        .join("run.json")
        .and_then(|path| path.read_to_string_capped(RUN_META_CAP))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Save `config` and its resolved host paths `paths` as the final config
/// of the last run in `run.json` of `sandbox_dir`.
pub fn save_last_run_config(
    sandbox_dir: &VerifiedPath,
    config: serde_json::Value,
    paths: HostPaths,
) -> anyhow::Result<()> {
    let mut meta = read_run_meta(sandbox_dir);
    meta.last_run_config = Some(config);
    meta.last_run_paths = Some(paths);
    write_run_meta(sandbox_dir, &meta)
}

/// Replace the config of the last run in `run.json` of `sandbox_dir`. Keeps
/// the host paths.
pub fn update_last_run_config(
    sandbox_dir: &VerifiedPath,
    config: serde_json::Value,
) -> anyhow::Result<()> {
    let mut meta = read_run_meta(sandbox_dir);
    meta.last_run_config = Some(config);
    write_run_meta(sandbox_dir, &meta)
}

/// Write `run.json` atomically (temp file and rename).
pub(super) fn write_run_meta(sandbox_dir: &VerifiedPath, meta: &RunMeta) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(meta)?;
    let tmp = sandbox_dir.join(".run.json.tmp")?;
    // Remove a stale temp file or a planted link. `create_new` never
    // follows a link that appears after the remove.
    match tmp.remove_file() {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    tmp.open_with(&opts)?.write_all(json.as_bytes())?;
    tmp.rename_to(&sandbox_dir.join("run.json")?)?;
    Ok(())
}
