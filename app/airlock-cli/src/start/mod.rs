//! The steps of `airlock start`.
//!
//! Prepares and runs a sandbox session for the current project:
//!  * checks that the host can run sandboxes
//!  * helps the user create a config for a new project
//!  * asks the user to approve new or changed user and project packs
//!  * finds `[env]` errors before the slow steps start
//!  * asks the user to accept config changes since the last run
//!  * prepares the sandbox and decides which tools to install
//!  * installs the tools that the project needs
//!  * starts the sandbox and runs the user's command in it

pub mod config_change;
pub mod env;
mod exit;
pub mod install;
pub mod location;
pub mod run;
pub mod sandbox;
pub mod wizard;

pub use exit::Exit;
use tracing::info;

use crate::cli::{self, LogLevel, logging};
use crate::data_dir::DataDir;
use crate::packs::approval::ApprovalMode;
use crate::util::VerifiedPath;

/// Command-line options for the sandbox step ([`sandbox::ensure_sandbox`]) and
/// the install step ([`install::install_tools`]).
#[allow(clippy::struct_excessive_bools)] // independent flags
pub struct SandboxOptions {
    /// Use the default answer for every sandbox question (`--yes`).
    pub yes: bool,
    /// A terminal is available for questions (see
    /// [`crate::cli::prompt::can_prompt`]).
    pub can_prompt: bool,
    /// Log level for the sandbox (`--log-level`).
    pub log_level: LogLevel,
    /// Show the install output (`--verbose`).
    pub verbose: bool,
    /// Accept config changes without a question
    /// (`--allow-configuration-changes`).
    pub allow_config_changes: bool,
    /// Config overrides (`--network`). They also apply to the config of
    /// the last run (see [`config_change::check`]).
    pub overrides: crate::config::ConfigOverrides,
    /// Approve new or changed packs without a question
    /// (`--auto-approve-packs`).
    pub auto_approve_packs: bool,
}

impl SandboxOptions {
    /// Return how the config resolve gets the approval of a new or changed
    /// pack. `--auto-approve-packs` approves all. Else it asks only in a
    /// terminal and without `--yes`: `--yes` never approves a pack.
    pub fn approval_mode(&self) -> ApprovalMode {
        if self.auto_approve_packs {
            ApprovalMode::Auto
        } else if self.can_prompt && !self.yes {
            ApprovalMode::Prompt
        } else {
            ApprovalMode::Refuse
        }
    }
}

/// Check that the host can run a sandbox. On Linux, this checks KVM access.
/// On failure, the process exits with code 1.
// Call this before anything is read or written.
pub fn check_system_requirements() {
    #[cfg(target_os = "linux")]
    crate::vm::require_kvm();
}

/// Check that the data directory `data_dir` is short enough for the Unix
/// sockets of the VM.
/// Returns:
///   Exit with code 1 and the fix, if the path is too long.
pub fn check_data_dir(data_dir: &DataDir) -> Result<(), Exit> {
    // On Linux, the VM sockets are in the sandbox directory
    // `<data_dir>/boxes/<id>/`. A Unix socket path has a limit of 107
    // bytes (108 with the trailing NUL). If a path is longer, virtiofsd
    // cannot make its socket, and the boot fails without a clear reason.
    // `/boxes/<id>` adds 15 bytes, and the longest socket name adds 16
    // (`/vfs/layers.sock`, `/vsock.sock_1024`). Thus 76 bytes are the
    // maximum. 70 keeps space for more mounts and new sockets. macOS does
    // not use Unix sockets for the VM.
    const MAX_DATA_DIR_LEN: usize = 70;

    let len = data_dir.root.given().as_os_str().len();
    if cfg!(target_os = "linux") && len > MAX_DATA_DIR_LEN {
        return Err(Exit::error(
            1,
            crate::messages::start::data_dir_too_long(
                &data_dir.root,
                len,
                MAX_DATA_DIR_LEN,
                crate::settings::Settings::expected_path(),
            ),
        ));
    }
    Ok(())
}

/// Return the current directory. It follows symlinks one time, so a
/// project below a symlink (for example macOS `/tmp`) has its real path.
pub fn resolve_host_cwd() -> Result<VerifiedPath, Exit> {
    VerifiedPath::current_dir()
        .map_err(|e| Exit::error(1, crate::messages::cli::cannot_determine_cwd(e)))
}

/// Initialize logging. The log goes to the sandbox directory when the
/// sandbox step knows it (see [`logging::attach`]).
/// Args:
///  - `level`: Log level for the log file
// This runs before the config files load. Thus, config loading, the setup
// wizard, config resolution and the later steps all write to the log.
pub fn init_logging(level: LogLevel) {
    logging::init(level);
    info!("airlock version {}", cli::version_string(true));
}

#[cfg(test)]
mod tests;
