//! Sandboxes.
//!
//! A sandbox keeps its data in the airlock data directory, out of the
//! reach of the sandbox guest. A registry maps each sandbox to its
//! project. Capabilities:
//!  * find the registered sandbox of a directory, its parents or the main
//!    repository of a git worktree, and register a new one
//!  * get the sandbox process of a sandbox: connect to a sandbox that
//!    runs, or start it. Then run processes in it, show the monitor, add
//!    port forwards and MCP servers, and release or stop it
//!  * the start work of `airlock start`: config, questions, installs and
//!    boot, in the sandbox process
//!  * the sandbox data: locks, run record, disk, CA and project config
//!  * remove a sandbox, and import a sandbox of an older version
//!
//! Only one process at a time starts a sandbox, and the kernel frees its
//! locks when the process exits.

/// Declare submodules with the visibility `$vis`. Tests can reach them
/// from all of the crate.
macro_rules! submodules {
    ($vis:vis $($name:ident),* $(,)?) => {$(
        #[cfg(not(test))]
        $vis mod $name;
        #[cfg(test)]
        pub(crate) mod $name;
    )*};
}

submodules!(
    disk,
    env,
    first_start,
    load,
    lock,
    lookup,
    meta,
    migrate,
    process,
    registry,
    remove,
    start,
);

pub use self::disk::{disk_id, ensure_disk, has_disk, reset_disk};
pub use self::env::{EnvError, MaskedSecret, SandboxEnv};
pub use self::load::{Project, ensure_cache_dir, open, resolve_env};
pub use self::lock::SandboxLock;
pub use self::lookup::{BOXES_DIR, FindOpts, Sandbox, Status, find, get, list};
pub use self::meta::{read_run_meta, time_ago};
pub use self::process::init::StartArgs;
pub use self::process::vm::Interrupted;
pub use self::process::vm::boot::{BootOptions, BootSpec, boot, guest_env};
pub use self::process::vm::io::{closed_stdin, drive};
pub use self::process::vm::machine::{ProcessSpec, Stopped, Vm};
pub use self::process::{
    ExecRequest, ExitedBeforeReady, ForwardMode, ForwardReport, McpRegistrar, Run, STOP_WAIT_SECS,
    SandboxProcess, StopResult, run_sandbox_process,
};
pub use self::registry::list as list_entries;
pub use self::start::check_env::check_env_early;
pub use self::start::config_change::approve_rule;
pub use self::start::ensure::{ChangedAnswer, ChangedPacks, changed_packs_answer};
pub use self::start::install::{InstallDecision, install_candidates, install_tools};
pub use self::start::location::find_or_register;
pub use self::start::wizard::run_template_wizard;
pub use self::start::{
    Exit, SandboxOptions, check_data_dir, check_system_requirements, init_logging, resolve_host_cwd,
};
#[cfg(test)]
mod tests;
