//! Pack install script.
//!
//! Makes the full install script of a pack from its setup script and the
//! shared shell helpers, and the command that runs it. Also explains the
//! known exit codes of the scripts in failure messages.

use crate::packs::InstallerScript;

/// Shared helpers of all install scripts.
const LIB: &str = include_str!("lib.sh");

/// Moves stdout to fd 3 (the status channel) and fd 1 to stderr.
///
/// Protocol v1 (`AIRLOCK_PACK_API=1`): after the wrapper, the stdout of the
/// exec carries only status lines (`steps <n>` and `status <text>`, written
/// by `airlock_steps` and `airlock_status`, see [`super::progress`]). The
/// stderr of the exec is the install log.
const WRAPPER: &str = "exec 3>&1 1>&2\n";

/// Linux limit for one exec string, with its end NUL (MAX_ARG_STRLEN). The
/// full install script is one argument of the exec, so it must fit.
pub const MAX_ARG_STRLEN: usize = 128 * 1024;

/// Get the maximum size of a `setup.sh` in bytes. With a larger file, the
/// full install script does not fit in one exec argument.
pub(in crate::packs) fn max_setup_len() -> usize {
    MAX_ARG_STRLEN - 1 - script("").len()
}

/// Make the full install script: wrapper, `lib.sh`, then `setup`.
/// Args:
///  - `setup`: The `setup.sh` of the pack
pub(in crate::packs) fn script(setup: &str) -> String {
    format!("{WRAPPER}{LIB}\n{setup}")
}

/// Make the argv that runs the install script with `/bin/sh -c`.
pub fn argv(installer: &InstallerScript) -> Vec<String> {
    vec![
        "/bin/sh".into(),
        "-c".into(),
        installer.script.clone(),
        format!("airlock-pack-{}", installer.pack),
    ]
}

/// Explain an install script exit code, for the failure message.
/// Returns:
///   A hint, or `None` for an exit code without a known meaning.
pub fn exit_hint(code: i32) -> Option<&'static str> {
    match code {
        10 => Some(crate::messages::packs::EXIT_HINT_IMAGE_UNSUPPORTED),
        11 => Some(crate::messages::packs::EXIT_HINT_PACKAGE_FAILED),
        12 => Some(crate::messages::packs::EXIT_HINT_DOWNLOAD_FAILED),
        13 => Some(crate::messages::packs::EXIT_HINT_ARG_INVALID),
        _ => None,
    }
}
