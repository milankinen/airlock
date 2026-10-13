//! The `airlock rm` command.
//!
//! Deletes the sandbox of the project in the current directory, in the
//! airlock data directory or in the project, with the local project config.
//! For a sandbox in the project, also deletes the local state in the
//! project.

use std::path::Path;

use clap::Args;

use crate::cli::prompt::choose;
use crate::config::files::EXTENSIONS;
use crate::context::Context;
use crate::sandboxes::{self, Location};
use crate::util::verified_path::VerifiedPath;
use crate::util::vfs;
use crate::{cli, messages, oci, project};

/// CLI arguments for `airlock rm`.
#[derive(Args, Debug)]
pub struct RmArgs {
    /// Do not ask for confirmation
    #[arg(short = 'f', long)]
    pub force: bool,
}

/// Remove the project sandbox after confirmation (unless `--force`).
///
/// A sandbox in the data directory goes with its registry entry and its
/// local project config. The project `.airlock/` directory stays.
///
/// For a sandbox in the project, removes the whole project `.airlock/`
/// directory: the sandbox, the local project config
/// (`.airlock/airlock.<ext>`), `.gitignore`, logs and all other files in it. If the project is the home directory, `.airlock/` is also the
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
pub async fn main(args: &RmArgs, context: &Context) -> i32 {
    let host_cwd = match sandboxes::project_dir(context) {
        Ok(cwd) => cwd,
        Err(e) => {
            cli::error!("{}", messages::cli::cannot_determine_cwd(e));
            return 1;
        }
    };
    run(args, context, &host_cwd).await
}

/// Body of [`main`], with the project directory as an argument.
// The argument lets tests run it without a change to the current directory
// of the process.
pub(super) async fn run(args: &RmArgs, context: &Context, host_cwd: &Path) -> i32 {
    match sandboxes::resolve_sandbox(context, host_cwd, false, false).await {
        Ok(Some(found)) => {
            if let Location::DataDir { id } = found.location {
                return rm_box(args, context, &id).await;
            }
        }
        Ok(None) => {}
        Err(e) => {
            cli::error!("{}", messages::cli::sandbox_lookup_failed(format!("{e:#}")));
            return 1;
        }
    }
    rm_project_sandbox(args, context, host_cwd)
}

/// Remove the sandbox `id` from the data directory (see
/// [`sandboxes::remove_box`]).
/// Returns:
///   Process exit code.
async fn rm_box(args: &RmArgs, context: &Context, id: &str) -> i32 {
    // Fail before the question. `remove_box` checks again under the lock.
    let running = sandboxes::boxes_dir(context)
        .and_then(|boxes| boxes.join(id))
        .is_ok_and(|dir| project::is_running(&dir));
    if running {
        cli::error!("{}", messages::sandboxes::SANDBOX_RUNNING_STOP_FIRST);
        return 1;
    }
    if !confirm(args) {
        cli::error!("{}", messages::cli::ABORTED);
        return 0;
    }
    if let Err(e) = sandboxes::remove_box(context, id).await {
        cli::error!(
            "{}",
            messages::cli::failed_to_remove_sandbox(format!("{e:#}"))
        );
        return 1;
    }
    oci::gc_sweep(&context.roots.data_dir);
    cli::log!("{}", messages::cli::SANDBOX_REMOVED);
    0
}

/// Remove the sandbox in the project `host_cwd` and the project
/// `.airlock/` directory (see [`main`]).
/// Args:
///  - `args`: Command arguments
///  - `context`: Process context
///  - `host_cwd`: Project directory
///
/// Returns:
///   Process exit code.
fn rm_project_sandbox(args: &RmArgs, context: &Context, host_cwd: &Path) -> i32 {
    let cache_dir = match VerifiedPath::new(host_cwd.join(".airlock"), &context.roots) {
        Ok(dir) => dir,
        Err(e) => {
            cli::error!("{}", messages::cli::failed_to_remove_sandbox(e));
            return 1;
        }
    };

    // `.airlock` can be a symlink. An untrusted repo can commit one that
    // points to, for example, `~/.airlock`. `start` refuses it (see
    // `project::airlock_dir_problem_for`). All code below builds paths from
    // `cache_dir`, and the file operations refuse a symlink in their path.
    // Thus handle the link separately and remove only the link.
    let Ok(cache_meta) = vfs::symlink_metadata(&cache_dir) else {
        return 0;
    };
    if cache_meta.file_type().is_symlink() {
        return rm_symlinked_airlock(args, &cache_dir);
    }
    let sandbox_dir = match cache_dir.join("sandbox") {
        Ok(dir) => dir,
        Err(e) => {
            cli::error!("{}", messages::cli::failed_to_remove_sandbox(e));
            return 1;
        }
    };

    let _lock = match project::lock_if_idle(&sandbox_dir) {
        project::IdleLock::Running => {
            cli::error!("{}", messages::sandboxes::SANDBOX_RUNNING_STOP_FIRST);
            return 1;
        }
        project::IdleLock::Held(file) => Some(file),
        project::IdleLock::Missing => None,
    };

    if is_user_home_project(&cache_dir) {
        return rm_sandbox_only(args, context, &sandbox_dir);
    }
    if let Some(marker) = user_file_marker(&cache_dir) {
        cli::log!(
            "{}",
            messages::cli::user_airlock_dir_warning(&cache_dir, marker)
        );
        return rm_sandbox_only(args, context, &sandbox_dir);
    }

    if !confirm(args) {
        cli::error!("{}", messages::cli::ABORTED);
        return 0;
    }

    // The check before use refuses `.airlock` if it changed to a symlink
    // or to an other directory while the prompt waited.
    if let Err(e) = vfs::remove_dir_all(&cache_dir) {
        cli::error!("{}", messages::cli::failed_to_remove_sandbox(e));
        return 1;
    }

    // The image hardlink of the sandbox was in its cache dir. Remove the
    // images and layers that no longer have live references.
    oci::gc_sweep(&context.roots.data_dir);

    cli::log!("{}", messages::cli::SANDBOX_REMOVED);
    0
}

/// Remove only `.airlock/sandbox`, for an `.airlock/` that is also a user
/// airlock directory (user config, vault, service sign-ins, agent homes).
///
/// This applies if the project is the home directory, or if
/// [`user_file_marker`] found a user-level file.
/// Args:
///  - `args`: Command arguments
///  - `context`: Process context
///  - `sandbox_dir`: The `.airlock/sandbox` directory to remove
///
/// Returns:
///   Process exit code.
pub(super) fn rm_sandbox_only(args: &RmArgs, context: &Context, sandbox_dir: &VerifiedPath) -> i32 {
    match vfs::symlink_metadata(sandbox_dir) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            cli::log!("{}", messages::cli::NO_SANDBOX_TO_REMOVE);
            return 0;
        }
        // For example a refused path: `.airlock` changed to a symlink.
        Err(e) => {
            cli::error!("{}", messages::cli::failed_to_remove_sandbox(e));
            return 1;
        }
    }

    if !confirm(args) {
        cli::error!("{}", messages::cli::ABORTED);
        return 0;
    }

    // `.airlock` can change to a symlink while the confirmation prompt
    // waits. The check before use refuses the path then, so the removal
    // cannot reach the link target.
    if let Err(e) = remove_entry(sandbox_dir) {
        cli::error!("{}", messages::cli::failed_to_remove_sandbox(e));
        return 1;
    }

    oci::gc_sweep(&context.roots.data_dir);
    cli::log!("{}", messages::cli::SANDBOX_REMOVED);
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
fn rm_symlinked_airlock(args: &RmArgs, cache_dir: &VerifiedPath) -> i32 {
    if !confirm(args) {
        cli::error!("{}", messages::cli::ABORTED);
        return 0;
    }

    if let Err(e) = vfs::remove_file(cache_dir) {
        cli::error!("{}", messages::cli::failed_to_remove_airlock_link(e));
        return 1;
    }

    cli::log!("{}", messages::cli::SANDBOX_REMOVED);
    0
}

/// Ask to confirm the removal of the sandbox. With `--force`, do not ask.
/// Returns:
///   True if confirmed. Abort, Esc, no terminal or a failed prompt is "no".
// All removals ask the same question and report the same result.
fn confirm(args: &RmArgs) -> bool {
    if args.force {
        return true;
    }
    matches!(
        choose::confirm(
            messages::cli::REMOVE_SANDBOX_TITLE,
            &[],
            messages::cli::REMOVE_SANDBOX_YES
        ),
        Ok(true)
    )
}

/// Return true if the project of `cache_dir` (`<project>/.airlock`) is the
/// home directory of a user. Then `cache_dir` is the `~/.airlock` of that
/// user.
// `$HOME` alone is not trusted, because `sudo` or a changed `HOME` can point
// it to a different directory. Thus the homes of the current user and of the
// owner of `cache_dir`, from the password database, also count.
fn is_user_home_project(cache_dir: &VerifiedPath) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(path) = cache_dir.to_entry_path() else {
        return false;
    };
    // SAFETY: getuid cannot fail.
    let current_uid = unsafe { libc::getuid() };
    let owner_uid = vfs::symlink_metadata(cache_dir).ok().map(|m| m.uid());
    dirs::home_dir()
        .into_iter()
        .chain(passwd_home(current_uid))
        .chain(owner_uid.and_then(passwd_home))
        .any(|home| is_home_dir(path, &home))
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
// The home dir can be a symlink, so the compare follows symlinks.
fn is_home_dir(cache_dir: &Path, home: &Path) -> bool {
    let canonical = |p: &Path| sandboxes::canonical(p).unwrap_or_else(|| p.to_path_buf());
    cache_dir
        .parent()
        .is_some_and(|project| canonical(project) == canonical(home))
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
fn user_file_marker(cache_dir: &VerifiedPath) -> Option<String> {
    let stems = USER_DIR_FILE_STEMS
        .iter()
        .flat_map(|stem| EXTENSIONS.iter().map(move |ext| format!("{stem}.{ext}")));
    USER_DIR_FILES
        .iter()
        .map(ToString::to_string)
        .chain(stems)
        .find(|name| {
            cache_dir
                .join(name)
                .and_then(|p| vfs::symlink_metadata(&p))
                .is_ok()
        })
}

/// Remove a file, symlink or directory tree without following symlinks.
fn remove_entry(path: &VerifiedPath) -> std::io::Result<()> {
    if vfs::symlink_metadata(path)?.is_dir() {
        vfs::remove_dir_all(path)
    } else {
        vfs::remove_file(path)
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
        let check =
            |path: &Path| is_user_home_project(&VerifiedPath::new_follow(path, None).unwrap());
        assert!(check(&home.join(".airlock")));
        assert!(!check(&home.join("proj/.airlock")));

        let uid = unsafe { libc::getuid() };
        if let Some(passwd_home) = passwd_home(uid) {
            assert!(check(&passwd_home.join(".airlock")));
        }
    }
}
