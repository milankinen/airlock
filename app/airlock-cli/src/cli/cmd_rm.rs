//! The `airlock rm` command.
//!
//! Deletes the project sandbox and its local state.

use std::path::Path;

use clap::Args;

use crate::cli::prompt::yes_no::YesNo;
use crate::config::files::EXTENSIONS;
use crate::{cli, oci, project};

/// CLI arguments for `airlock rm`.
#[derive(Args, Debug)]
pub struct RmArgs {
    /// Do not ask for confirmation
    #[arg(short = 'f', long)]
    pub force: bool,
}

/// Remove the project sandbox after confirmation (unless `--force`).
///
/// Removes the whole project `.airlock/` directory: the sandbox, the local
/// project config (`.airlock/airlock.<ext>`), `.gitignore`, logs and all other
/// files in it. If the project is the home directory, `.airlock/` is also the
/// user airlock directory (user config, vault, service sign-ins, agent homes).
/// Then only `.airlock/sandbox` is removed. The same applies if `.airlock/`
/// holds user-level files and the project is not `$HOME` (for example under
/// `sudo`, or with a wrong `$HOME`).
/// Returns:
///   Process exit code: 0 on success or abort, 1 on error.
// The config is not loaded. `rm` needs only the sandbox paths, and a broken
// config is a common reason to start again.
//
// The sandbox lock stays held until the removal is complete. Thus a parallel
// `airlock start` cannot take the sandbox during the removal.
pub fn main(args: &RmArgs) -> i32 {
    let host_cwd = match std::env::current_dir() {
        Ok(cwd) => std::fs::canonicalize(&cwd).unwrap_or(cwd),
        Err(e) => {
            cli::error!("Cannot determine current directory: {e}");
            return 1;
        }
    };
    run(args, &host_cwd)
}

/// Body of [`main`], with the project directory as an argument.
// The argument lets tests run it without a change to the current directory
// of the process.
pub(super) fn run(args: &RmArgs, host_cwd: &Path) -> i32 {
    let paths = project::paths(host_cwd);

    // `.airlock` can be a symlink. An untrusted repo can commit one that
    // points to, for example, `~/.airlock`. `start` refuses it (see
    // `project::airlock_dir_problem_for`). All code below builds paths from
    // `cache_dir`. If `rm` followed the link, it could delete the link
    // target. Thus handle the link separately and never resolve through it.
    let Ok(cache_meta) = std::fs::symlink_metadata(&paths.cache_dir) else {
        return 0;
    };
    if cache_meta.file_type().is_symlink() {
        return rm_symlinked_airlock(args, &paths.cache_dir);
    }

    let _lock = match project::lock_if_idle(&paths.sandbox_dir) {
        project::IdleLock::Running => {
            cli::error!("Sandbox is running, stop it first");
            return 1;
        }
        project::IdleLock::Held(file) => Some(file),
        project::IdleLock::Missing => None,
    };

    if is_user_home_project(&paths.cache_dir) {
        let kept_note = "the other files in ~/.airlock: the project is the home directory";
        return rm_sandbox_only(args, &paths.cache_dir, &paths.sandbox_dir, kept_note);
    }
    if let Some(marker) = user_file_marker(&paths.cache_dir) {
        cli::log!(
            "warning: {} holds the user-level airlock file {marker}; \
             it is a user airlock directory, so only its sandbox goes",
            paths.cache_dir.display()
        );
        let kept_note =
            format!("the other files in .airlock: it holds user-level files ({marker})");
        return rm_sandbox_only(args, &paths.cache_dir, &paths.sandbox_dir, &kept_note);
    }

    let local_config = local_config_name(&paths.cache_dir);
    let note = local_config
        .as_ref()
        .map(|name| format!("This also deletes the local config .airlock/{name}."));
    if !confirm(args, note.as_deref()) {
        cli::error!("Aborted.");
        return 0;
    }

    if let Err(e) = std::fs::remove_dir_all(&paths.cache_dir) {
        cli::error!("Failed to remove sandbox: {e}");
        return 1;
    }

    // The image hardlink of the sandbox was in its cache dir. Remove the
    // images and layers that no longer have live references.
    oci::gc_sweep();

    match local_config {
        Some(name) => cli::log!("Sandbox removed (including .airlock/{name})"),
        None => cli::log!("Sandbox removed"),
    }
    0
}

/// Remove only `.airlock/sandbox`, for an `.airlock/` that is also a user
/// airlock directory (user config, vault, service sign-ins, agent homes).
///
/// This applies if the project is the home directory, or if
/// [`user_file_marker`] found a user-level file.
/// Args:
///  - `args`: Command arguments
///  - `cache_dir`: Project `.airlock/` directory
///  - `sandbox_dir`: The `.airlock/sandbox` directory to remove
///  - `kept_note`: Message that tells what stays and why
///
/// Returns:
///   Process exit code.
fn rm_sandbox_only(args: &RmArgs, cache_dir: &Path, sandbox_dir: &Path, kept_note: &str) -> i32 {
    if std::fs::symlink_metadata(sandbox_dir).is_err() {
        cli::log!("No sandbox to remove (kept {kept_note})");
        return 0;
    }

    if !confirm(args, None) {
        cli::error!("Aborted.");
        return 0;
    }

    // `sandbox_dir` is `cache_dir.join("sandbox")`. Check again, immediately
    // before the removal, that `.airlock` is still a real directory. It can
    // change to a symlink while the confirmation prompt waits. Without this
    // check, the path resolves through the new link and the removal below
    // reaches the link target.
    match std::fs::symlink_metadata(cache_dir) {
        Ok(meta) if !meta.file_type().is_symlink() => {}
        _ => {
            cli::error!("{} is now a symbolic link; aborting", cache_dir.display());
            return 1;
        }
    }

    if let Err(e) = remove_entry(sandbox_dir) {
        cli::error!("Failed to remove sandbox: {e}");
        return 1;
    }

    oci::gc_sweep();
    cli::log!("Sandbox removed (kept {kept_note})");
    0
}

/// Remove a symlinked `.airlock` (see [`run`]).
///
/// Removes only the link, after the normal confirmation (no confirmation with
/// `--force`). The target can be the real `.airlock` of another project or of
/// a user. The function never changes the target and never follows the link
/// to look inside.
/// Returns:
///   Process exit code.
fn rm_symlinked_airlock(args: &RmArgs, cache_dir: &Path) -> i32 {
    let target =
        std::fs::read_link(cache_dir).map_or_else(|_| "?".to_string(), |p| p.display().to_string());

    if !confirm_prompt(
        args,
        &format!(
            "Remove the .airlock link? It is a symbolic link to {target}; the target is left \
             untouched."
        ),
    ) {
        cli::error!("Aborted.");
        return 0;
    }

    if let Err(e) = std::fs::remove_file(cache_dir) {
        cli::error!("Failed to remove the .airlock link: {e}");
        return 1;
    }

    cli::log!("Removed the .airlock link to {target}; its target was not touched");
    0
}

/// Ask for confirmation with the standard "Remove sandbox?" prompt.
/// Args:
///  - `args`: Command arguments
///  - `note`: Optional text after the prompt about other effects of the
///    removal, for example that the local project config is also removed
///
/// Returns:
///   True if the user confirmed (see [`confirm_prompt`]).
fn confirm(args: &RmArgs, note: Option<&str>) -> bool {
    let prompt = match note {
        Some(note) => format!("Remove sandbox? {note}"),
        None => "Remove sandbox?".to_string(),
    };
    confirm_prompt(args, &prompt)
}

/// Ask `prompt` to confirm a removal. With `--force`, do not ask.
/// Returns:
///   True if confirmed. Esc, no terminal or a failed prompt is "no".
// Removals that are not "the sandbox" (for example a symlinked `.airlock`)
// call this directly. Other removals call it through [`confirm`].
fn confirm_prompt(args: &RmArgs, prompt: &str) -> bool {
    if args.force {
        return true;
    }
    let question = YesNo {
        question: prompt,
        default: false,
    };
    matches!(question.ask(), Ok(Some(true)))
}

/// Return true if the project of `cache_dir` (`<project>/.airlock`) is the
/// home directory of a user. Then `cache_dir` is the `~/.airlock` of that
/// user.
// `$HOME` alone is not trusted, because `sudo` or a changed `HOME` can point
// it to a different directory. Thus the homes of the current user and of the
// owner of `cache_dir`, from the password database, also count.
fn is_user_home_project(cache_dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid cannot fail.
    let current_uid = unsafe { libc::getuid() };
    let owner_uid = std::fs::symlink_metadata(cache_dir).ok().map(|m| m.uid());
    dirs::home_dir()
        .into_iter()
        .chain(passwd_home(current_uid))
        .chain(owner_uid.and_then(passwd_home))
        .any(|home| is_home_dir(cache_dir, &home))
}

/// Return the home directory of `uid` from the password database.
fn passwd_home(uid: libc::uid_t) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: an all-zero passwd is a valid value for getpwuid_r to fill.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call. `buf` lives longer than
    // the strings in `entry`, and the code reads them only below.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &raw mut entry,
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut found,
        )
    };
    if rc != 0 || found.is_null() || entry.pw_dir.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r succeeded, so pw_dir is a NUL-terminated string in `buf`.
    let dir = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
    Some(std::ffi::OsStr::from_bytes(dir.to_bytes()).into())
}

/// Return true if the parent of `cache_dir` is `home` (canonical paths).
fn is_home_dir(cache_dir: &Path, home: &Path) -> bool {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    cache_dir
        .parent()
        .is_some_and(|project| canonical(project) == canonical(home))
}

/// Return the file name of the local project config (`airlock.<ext>`) in
/// `cache_dir`, if it exists.
// Only the confirmation prompt uses the name. The removal deletes the whole
// directory in all cases.
fn local_config_name(cache_dir: &Path) -> Option<String> {
    EXTENSIONS.iter().find_map(|ext| {
        let name = format!("airlock.{ext}");
        cache_dir.join(&name).is_file().then_some(name)
    })
}

/// Files and directories that only a user airlock directory (`~/.airlock`)
/// holds, never a project `.airlock/`.
///
/// These are the file vaults, the airlock database (with the token store of
/// the network services) and the homes that the agent presets of the list
/// form mount (`claude`, `codex`). No current pack uses `agents`. The agent
/// packs keep their homes in the pack mount directories, not here.
/// `airlock.<ext>` is not a marker, because a project also has one.
const USER_DIR_FILES: [&str; 6] = [
    "vault.default.json",
    "vault.default.enc.json",
    crate::db::DIR,
    "claude",
    "codex",
    "agents",
];

/// Stems of the user-only config files `<stem>.<ext>`: application settings
/// (`settings.<ext>`) and the user config (`config.<ext>`).
const USER_DIR_FILE_STEMS: [&str; 2] = ["settings", "config"];

/// Return the first user-level airlock file in `cache_dir` (see
/// [`USER_DIR_FILES`] and [`USER_DIR_FILE_STEMS`]). Return `None` if it looks
/// like a project `.airlock/`.
fn user_file_marker(cache_dir: &Path) -> Option<String> {
    let stems = USER_DIR_FILE_STEMS
        .iter()
        .flat_map(|stem| EXTENSIONS.iter().map(move |ext| format!("{stem}.{ext}")));
    USER_DIR_FILES
        .iter()
        .map(ToString::to_string)
        .chain(stems)
        .find(|name| std::fs::symlink_metadata(cache_dir.join(name)).is_ok())
}

/// Remove a file, symlink or directory tree without following symlinks.
fn remove_entry(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the check that finds a project in the home directory.

    use super::*;
    use crate::test_cfg::home::TempHome;

    /// Test that the home check finds `.airlock` in the home directory and not
    /// in a project below it. In the home, `.airlock` also holds user files.
    ///   1. Set `$HOME` to a temp directory with a project below it
    ///   2. Check that the full check finds `.airlock` in `$HOME` but not in
    ///      the project
    ///   3. If the password database has a home for the user, check that the
    ///      full check finds `.airlock` in it
    #[test]
    fn home_project_is_found_by_env_home_or_password_database() {
        let home = TempHome::new();
        let home = home.path();
        std::fs::create_dir_all(home.join(".airlock")).unwrap();
        std::fs::create_dir_all(home.join("proj/.airlock")).unwrap();
        assert!(is_user_home_project(&home.join(".airlock")));
        assert!(!is_user_home_project(&home.join("proj/.airlock")));

        let uid = unsafe { libc::getuid() };
        if let Some(passwd_home) = passwd_home(uid) {
            assert!(is_user_home_project(&passwd_home.join(".airlock")));
        }
    }
}
