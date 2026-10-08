//! The steps of `airlock start`.
//!
//! Prepares and runs a sandbox session for the current project:
//!  * checks that the host can run sandboxes
//!  * helps the user create a config for a new project
//!  * finds `[env]` errors before the slow steps start
//!  * prepares the sandbox and decides which tools to install
//!  * installs the tools that the project needs
//!  * starts the sandbox and runs the user's command in it

pub mod env;
mod exit;
pub mod install;
pub mod run;
pub mod sandbox;
pub mod wizard;

use std::path::{Path, PathBuf};

pub use exit::Exit;
use tracing::info;

use crate::cli::{self, LogLevel, logging};
use crate::project;

/// Command-line options for the sandbox step ([`sandbox::ensure_sandbox`]) and
/// the install step ([`install::install_tools`]).
pub struct SandboxOptions {
    /// Use the default answer for every sandbox question (`--yes`).
    pub yes: bool,
    /// Log level for the sandbox (`--log-level`).
    pub log_level: LogLevel,
    /// Show the install output (`--verbose`).
    pub verbose: bool,
}

/// Check that the host can run a sandbox. On Linux, this checks KVM access.
/// On failure, the process exits with code 1.
// Call this before anything is read or written.
pub fn check_system_requirements() {
    #[cfg(target_os = "linux")]
    crate::vm::require_kvm();
}

/// Return the current directory. Make the path canonical when possible.
pub fn resolve_host_cwd() -> Result<PathBuf, Exit> {
    match std::env::current_dir() {
        Ok(p) => Ok(std::fs::canonicalize(&p).unwrap_or(p)),
        Err(e) => Err(Exit::error(
            1,
            format!("Cannot determine current directory: {e}"),
        )),
    }
}

/// Create `.airlock/` and initialize logging there.
/// Args:
///  - `host_cwd`: Project directory on the host
///  - `level`: Log level for the log file
///
/// Returns:
///   Error if the directory creation fails.
// This runs before the config files load. Thus, config loading, the setup
// wizard, config resolution and the later steps all write to the log.
pub fn init_logging(host_cwd: &Path, level: LogLevel) -> Result<(), Exit> {
    let cache_dir = project::ensure_cache_dir(host_cwd)
        .map_err(|e| Exit::error(1, format!("Failed to create .airlock directory: {e}")))?;
    logging::init(level, &cache_dir);
    info!("airlock version {}", cli::version_string(true));
    Ok(())
}

#[cfg(test)]
mod tests;
