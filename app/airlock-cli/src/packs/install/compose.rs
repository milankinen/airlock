//! The install script of one pack: a wrapper, `lib.sh`, then the pack's
//! `setup.sh`, run with `/bin/sh -c` in the install boot.
//!
//! Protocol v1 (`AIRLOCK_PACK_API=1`): the wrapper moves the exec's stdout
//! to fd 3 and sends everything else to stderr (`exec 3>&1 1>&2`). So the
//! exec's stdout carries only status lines (`steps <n>` and
//! `status <text>`, written by `airlock_steps` and `airlock_status`; see
//! [`super::progress`]), and its stderr is the install log.

use crate::packs::InstallerScript;

/// The shared helpers of every install script.
const LIB: &str = include_str!("lib.sh");

/// Moves stdout to fd 3 (the status channel) and fd 1 to stderr.
const WRAPPER: &str = "exec 3>&1 1>&2\n";

/// The whole install script of the setup script `setup`: wrapper,
/// `lib.sh`, `setup`.
pub(in crate::packs) fn script(setup: &str) -> String {
    format!("{WRAPPER}{LIB}\n{setup}")
}

/// The argv of the install exec of `installer`.
pub fn argv(installer: &InstallerScript) -> Vec<String> {
    vec![
        "/bin/sh".into(),
        "-c".into(),
        installer.script.clone(),
        format!("airlock-pack-{}", installer.pack),
    ]
}

/// What an install script exit code means, for the failure message.
pub fn exit_hint(code: i32) -> Option<&'static str> {
    match code {
        10 => Some("the image is not Alpine- or Debian-based, or the CPU is not supported"),
        11 => Some("a package could not be installed; see the log"),
        12 => Some(
            "a download failed; check the network (GitHub rate limits show as HTTP 403) \
             and run `airlock start` again",
        ),
        13 => Some("an arg value of the pack is not valid; check the [packs] args"),
        _ => None,
    }
}
