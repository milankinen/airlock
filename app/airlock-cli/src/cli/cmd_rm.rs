//! `airlock rm` — delete a sandbox's cached state.

use std::path::Path;

use clap::Args;

use crate::cli::prompt::yes_no::YesNo;
use crate::config::files::EXTENSIONS;
use crate::{cli, oci, project};

/// CLI arguments for `airlock rm`.
#[derive(Args, Debug)]
pub struct RmArgs {
    /// Skip confirmation prompt
    #[arg(short = 'f', long)]
    pub force: bool,
}

/// Remove the sandbox directory after confirmation (unless `--force`).
///
/// Removes the whole project `.airlock/` directory: the sandbox, the local
/// project config (`.airlock/airlock.<ext>`), `.gitignore`, logs — everything
/// under it. When the project is the home directory, `.airlock/` is also the
/// user's airlock directory (user config, vault, service sign-ins, agent
/// homes): only `.airlock/sandbox` goes then. The same applies when
/// `.airlock/` holds user-level files although the project is not `$HOME`
/// (for example under `sudo`, or with a wrong `$HOME`).
///
/// The config is not loaded: `rm` needs only the sandbox paths, and a
/// broken config is a common reason to start over.
///
/// The sandbox lock is held until the removal finishes, so a concurrent
/// `airlock start` cannot take the sandbox while it is being removed.
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

/// [`main`]'s body, taking the project directory as an argument so tests
/// can drive it without touching the process's current directory.
pub(super) fn run(args: &RmArgs, host_cwd: &Path) -> i32 {
    let paths = project::paths(host_cwd);

    // `.airlock` itself may be a symlink (an untrusted repo can commit one
    // pointing at, say, `~/.airlock`): `start` refuses it (see
    // `project::airlock_dir_problem_for`), and everything below this point
    // builds paths by joining onto `cache_dir`, so following it here would
    // let `rm` reach — and delete — whatever the link points at. Handle it
    // on its own, without ever resolving through it.
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

    // The sandbox's image hardlink went away with its cache dir — sweep any
    // image/layers that no longer have live refs.
    oci::gc_sweep();

    match local_config {
        Some(name) => cli::log!("Sandbox removed (including .airlock/{name})"),
        None => cli::log!("Sandbox removed"),
    }
    0
}

/// Remove only `.airlock/sandbox`, for an `.airlock/` that is also a user
/// airlock directory (user config, vault, service sign-ins, agent homes):
/// the project is
/// the home directory, or [`user_file_marker`] found a user-level file.
/// `kept_note` says what stays and why.
fn rm_sandbox_only(args: &RmArgs, cache_dir: &Path, sandbox_dir: &Path, kept_note: &str) -> i32 {
    if std::fs::symlink_metadata(sandbox_dir).is_err() {
        cli::log!("No sandbox to remove (kept {kept_note})");
        return 0;
    }

    if !confirm(args, None) {
        cli::error!("Aborted.");
        return 0;
    }

    // `sandbox_dir` is `cache_dir.join("sandbox")`: re-check that `.airlock`
    // is still a real directory right before the removal, in case it was
    // swapped for a symlink while the confirmation prompt above was
    // waiting. Without this, the path built from it would resolve through
    // the swapped-in link and the removal below would reach its target.
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

/// Remove a symlinked `.airlock` (see [`run`]): only the link goes, after
/// the normal confirmation (skipped with `--force`); its target, which may
/// be another project's or a user's real `.airlock`, is never touched or
/// even followed to look inside.
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

/// [`confirm_prompt`] with the standard "Remove sandbox?" prompt. `note`,
/// when given, is appended to flag extra fallout of the removal, such as
/// the local project config going with it.
fn confirm(args: &RmArgs, note: Option<&str>) -> bool {
    let prompt = match note {
        Some(note) => format!("Remove sandbox? {note}"),
        None => "Remove sandbox?".to_string(),
    };
    confirm_prompt(args, &prompt)
}

/// Ask `prompt` to confirm removal (skipped with `--force`). Esc, no
/// terminal or a failed prompt is "no". Used directly by removals that
/// are not "the sandbox" (e.g. a symlinked `.airlock`), and through
/// [`confirm`] otherwise.
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

/// Whether `cache_dir` (`<project>/.airlock`) is `<home>/.airlock`.
/// Whether the project of `cache_dir` is a user's home directory, so
/// `cache_dir` is that user's `~/.airlock`. `$HOME` alone is not trusted
/// (`sudo` or an overridden `HOME` point it elsewhere): the homes of the
/// current user and of the owner of `cache_dir`, from the password
/// database, count too.
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

/// The home directory of `uid` in the password database.
fn passwd_home(uid: libc::uid_t) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: an all-zero passwd is a valid value for getpwuid_r to fill.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call; `buf` outlives the
    // strings in `entry`, which are only read below.
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

fn is_home_dir(cache_dir: &Path, home: &Path) -> bool {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    cache_dir
        .parent()
        .is_some_and(|project| canonical(project) == canonical(home))
}

/// The local project config's file name (`airlock.<ext>`) in `cache_dir`,
/// if one exists. Used only to mention it in the confirmation prompt:
/// removal takes the whole directory regardless.
fn local_config_name(cache_dir: &Path) -> Option<String> {
    EXTENSIONS.iter().find_map(|ext| {
        let name = format!("airlock.{ext}");
        cache_dir.join(&name).is_file().then_some(name)
    })
}

/// Files and directories that only a user airlock directory (`~/.airlock`)
/// holds, never a project `.airlock/`: the file vaults, the airlock
/// database (with the token store of the network services), and the homes that the agent packs mount
/// (`claude`, `codex` of the list form, `agents/codex` of `codex@1`).
/// `airlock.<ext>` is not a marker: a project has one too.
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

/// The first user-level airlock file found in `cache_dir` (see
/// [`USER_DIR_FILES`] and [`USER_DIR_FILE_STEMS`]), or `None` when it looks
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
    use super::*;
    use crate::test_cfg::temp_dir;

    #[test]
    fn home_project_is_found_by_env_home_or_password_database() {
        let tmp = temp_dir();
        let home = tmp.path();
        std::fs::create_dir_all(home.join("proj/.airlock")).unwrap();
        assert!(is_home_dir(&home.join(".airlock"), home));
        assert!(!is_home_dir(&home.join("proj/.airlock"), home));

        let uid = unsafe { libc::getuid() };
        if let Some(passwd_home) = passwd_home(uid) {
            assert!(is_user_home_project(&passwd_home.join(".airlock")));
        }
    }
}
