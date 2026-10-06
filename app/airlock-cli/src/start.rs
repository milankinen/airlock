//! The steps of `airlock start`: the system check
//! ([`check_system_requirements`]), the setup wizard for a project without
//! config ([`wizard`]), the stored sandbox with the tool decisions
//! ([`sandbox`], with the early `[env]` check of [`env`]), the tools'
//! install ([`install`]), and the sandbox session ([`run`]).

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

/// What the sandbox step ([`sandbox::ensure_sandbox`]) and the install
/// step ([`install::install_tools`]) take from the command line.
pub struct SandboxOptions {
    /// Answer every sandbox question with its default (`--yes`).
    pub yes: bool,
    pub log_level: LogLevel,
    /// Show the install output (`--verbose`).
    pub verbose: bool,
}

/// Check that the host can run a sandbox: KVM on Linux (exit code 1).
/// Nothing is read or written before it.
pub fn check_system_requirements() {
    #[cfg(target_os = "linux")]
    crate::vm::require_kvm();
}

/// The current directory, canonical when possible.
pub fn resolve_host_cwd() -> Result<PathBuf, Exit> {
    match std::env::current_dir() {
        Ok(p) => Ok(std::fs::canonicalize(&p).unwrap_or(p)),
        Err(e) => Err(Exit::error(
            1,
            format!("Cannot determine current directory: {e}"),
        )),
    }
}

/// Create `.airlock/` and initialize logging there. It runs before the
/// config files load, so the config loading, the setup wizard, the
/// config resolution and the later steps log to it.
pub fn init_logging(host_cwd: &Path, level: LogLevel) -> Result<(), Exit> {
    let cache_dir = project::ensure_cache_dir(host_cwd)
        .map_err(|e| Exit::error(1, format!("Failed to create .airlock directory: {e}")))?;
    logging::init(level, &cache_dir);
    info!("airlock version {}", cli::version_string(true));
    Ok(())
}
