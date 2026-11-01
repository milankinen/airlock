//! Install of the macOS notifier app.
//!
//! The airlock binary contains the notifier app bundle as an archive.
//! Airlock installs the app to one fixed place in the data directory and
//! keeps a version mark next to it. When the mark and the archive do not
//! agree, airlock unpacks the archive to a temporary directory and then
//! moves the new app into place. Thus no process sees a partly unpacked
//! app. A lock lets only one process install at a time. The other
//! processes wait for it. The archive can contain only plain files and
//! directories.
//!
//! Airlock also removes the temporary files of stopped processes, and the
//! notifier directory of older airlock versions.

// Only macOS uses the notifier app. The tests check the install on all
// hosts.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::bail;
use sha2::{Digest, Sha256};

use crate::data_dir;
use crate::messages::grants as msg;
use crate::util::VerifiedPath;

/// Name of the app bundle in the archive and in the support directory.
pub(super) const APP: &str = "AirlockNotifier.app";
/// Path of the executable in the app bundle.
pub(super) const EXE: &str = "Contents/MacOS/airlock-macos-notifier";
/// Name of the version mark of the installed app.
const VERSION_FILE: &str = "AirlockNotifier.version";
/// Name of the install lock file.
const LOCK_FILE: &str = "AirlockNotifier.lock";
/// Name prefix of the temporary directories. The process ID follows it.
const TEMP_PREFIX: &str = ".tmp.";
/// Maximum wait time for the install of another process.
const LOCK_WAIT: Duration = Duration::from_secs(10);
/// Time between two tries to get the install lock.
const LOCK_POLL: Duration = Duration::from_millis(50);

/// Installed notifier app.
#[derive(Debug)]
pub(super) struct Installed {
    /// The app bundle.
    pub app: VerifiedPath,
    /// True if this call unpacked the app.
    pub fresh: bool,
}

/// Install the notifier app if the installed version is not the version
/// of the archive. Waits while another process installs.
/// Args:
///  - `dir`: Support directory in the data directory
///  - `tar`: Archive of the app bundle
///
/// Returns:
///   The installed app, or error if the unpack fails, the archive does not
///   have a complete app or another process installs for too long.
pub(super) fn install(dir: &VerifiedPath, tar: &[u8]) -> anyhow::Result<Installed> {
    let dir = data_dir::make_dir(dir)?;
    let version = version_name(tar);
    let app = dir.join(APP)?;
    let mark = dir.join(VERSION_FILE)?;
    let _lock = lock(&dir.join(LOCK_FILE)?)?;
    let installed = mark.read_to_string_capped(256).ok();
    if installed.as_deref() == Some(version.as_str()) && check_app(&dir).is_ok() {
        return Ok(Installed { app, fresh: false });
    }
    remove_stale_temps(&dir);
    let staging = private_temp_dir(&dir)?;
    let result = replace(&staging, &app, &mark, &version, tar);
    let _ = staging.remove_dir_all();
    result?;
    Ok(Installed { app, fresh: true })
}

/// Unpack the archive to `staging` and move the new app to `app`. The old
/// app moves to `staging`, for removal with it.
fn replace(
    staging: &VerifiedPath,
    app: &VerifiedPath,
    mark: &VerifiedPath,
    version: &str,
    tar: &[u8],
) -> anyhow::Result<()> {
    unpack(tar, staging)?;
    check_app(staging)?;
    let new_mark = staging.join(VERSION_FILE)?;
    new_mark.write(version)?;
    // Remove the mark first: after a crash in the next steps, the next
    // process installs again.
    match mark.remove_file() {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    // A rename cannot replace a directory that is not empty. Thus move the
    // old app away first.
    if app.symlink_metadata().is_ok() {
        app.rename_to(&staging.join("old")?)?;
    }
    staging.join(APP)?.rename_to(app)?;
    new_mark.rename_to(mark)?;
    Ok(())
}

/// Get the install lock. Waits up to [`LOCK_WAIT`] while another process
/// holds it.
///
/// Returns:
///   The lock, which stays until it drops, or error if the wait time ends.
fn lock(path: &VerifiedPath) -> anyhow::Result<File> {
    let file = path.open_with(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600),
    )?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        // SAFETY: The descriptor is open for the time of the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::WouldBlock {
            return Err(err.into());
        }
        if Instant::now() >= deadline {
            bail!(msg::NOTIFIER_BUSY);
        }
        std::thread::sleep(LOCK_POLL);
    }
}

/// Remove the notifier directory of older airlock versions. Best effort:
/// errors are ignored.
/// Args:
///  - `old`: Old notifier directory in the data directory
///  - `before_remove`: Called with each old app bundle before the removal
pub(super) fn remove_legacy(old: &VerifiedPath, before_remove: impl Fn(&VerifiedPath)) {
    let Ok(entries) = old.read_dir() else {
        return;
    };
    for name in entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
    {
        if let Ok(app) = old.join(&name).and_then(|d| d.join(APP))
            && app.is_dir()
        {
            before_remove(&app);
        }
    }
    let _ = old.remove_dir_all();
}

/// Remove the temporary directories of stopped processes. Best effort:
/// errors are ignored.
fn remove_stale_temps(dir: &VerifiedPath) {
    let Ok(entries) = dir.read_dir() else {
        return;
    };
    let names = entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok());
    for name in stale_temps(names, pid_alive) {
        if let Ok(path) = dir.join(name) {
            let _ = path.remove_dir_all();
        }
    }
}

/// Make a new private temporary directory in `dir`. Its name has the
/// process ID, so that a later cleanup knows when it is not in use.
pub(super) fn private_temp_dir(dir: &VerifiedPath) -> anyhow::Result<VerifiedPath> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{TEMP_PREFIX}{}.{n}", std::process::id()))?;
    // A directory of a stopped process with the same ID can remain.
    match path.remove_dir_all() {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    data_dir::create_private_dir(&path)
}

/// Get the version of the archive: a hash of its content.
fn version_name(tar: &[u8]) -> String {
    hex::encode(Sha256::digest(tar))
}

/// Select the temporary directories to remove.
/// Args:
///  - `names`: Names in the support directory
///  - `alive`: Tells if a process ID is in use
///
/// Returns:
///   The temporary directories of stopped processes. Other names stay.
fn stale_temps(
    names: impl IntoIterator<Item = String>,
    alive: impl Fn(u32) -> bool,
) -> Vec<String> {
    names
        .into_iter()
        .filter(|name| {
            name.strip_prefix(TEMP_PREFIX)
                .and_then(|rest| rest.split('.').next())
                .and_then(|pid| pid.parse().ok())
                .is_some_and(|pid| !alive(pid))
        })
        .collect()
}

/// Return `true` if a process with the ID exists.
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: Signal 0 only checks the process. It sends nothing.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Check that `root` has the app bundle with its executable.
fn check_app(root: &VerifiedPath) -> anyhow::Result<()> {
    let exe = root.join(APP)?.join(EXE)?;
    match exe.metadata() {
        Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => Ok(()),
        _ => bail!(msg::NOTIFIER_INCOMPLETE),
    }
}

/// Unpack the app bundle from the archive to `dest`. Refuses links and
/// other special entries, and paths outside the app bundle.
fn unpack(tar: &[u8], dest: &VerifiedPath) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(tar);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        let raw = entry.path()?.to_string_lossy().into_owned();
        let Some(rel) = bundle_path(&raw) else {
            bail!(msg::notifier_bad_entry(&raw));
        };
        // `join` refuses a symlink before the last name. The archive cannot
        // make one, because it can have only files and directories.
        let path = dest.join(&rel)?;
        // Only plain files and directories. Links, sparse and contiguous
        // files and devices are refused.
        match kind {
            tar::EntryType::Directory => path.mkdirs()?,
            tar::EntryType::Regular => {
                if let Some(parent) = path.parent() {
                    parent.mkdirs()?;
                }
                let mode = if entry.header().mode()? & 0o111 == 0 {
                    0o644
                } else {
                    0o755
                };
                let mut file =
                    path.open_with(OpenOptions::new().write(true).create_new(true).mode(mode))?;
                io::copy(&mut entry, &mut file)?;
            }
            _ => bail!(msg::notifier_bad_entry(&raw)),
        }
    }
    Ok(())
}

/// Check the path of an archive entry.
///
/// Returns:
///   The clean relative path, or `None` if the path is absolute, has a
///   `..` component or is not in the app bundle.
fn bundle_path(raw: &str) -> Option<String> {
    if raw.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = raw
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.first() != Some(&APP) || parts.contains(&"..") {
        return None;
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_cfg::oci::verified_dir;
    use crate::test_cfg::temp_dir;

    /// Make an app archive with the executable and a resource file.
    fn app_tar(exe: &[u8]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        let mut add = |path: &str, data: &[u8], mode: u32, kind: tar::EntryType| {
            let mut header = tar::Header::new_ustar();
            header.set_entry_type(kind);
            header.set_mode(mode);
            header.set_size(data.len() as u64);
            tar.append_data(&mut header, path, data).unwrap();
        };
        add(APP, b"", 0o755, tar::EntryType::Directory);
        add(
            &format!("{APP}/Contents/Info.plist"),
            b"plist",
            0o644,
            tar::EntryType::Regular,
        );
        add(&format!("{APP}/{EXE}"), exe, 0o755, tar::EntryType::Regular);
        tar.into_inner().unwrap()
    }

    /// Get the sorted names in `dir`.
    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Test that the install unpacks the app once per version, keeps the
    /// exec bit, and replaces the app in the same place for a new version.
    ///   1. Install version 1 and check the files and the exec bit
    ///   2. Install version 1 again and check that it does not unpack
    ///   3. Install version 2 and check that it replaces version 1
    ///   4. Check that only the app, the mark and the lock stay
    #[test]
    fn install_unpacks_once_and_replaces_app_of_other_version() {
        let tmp = temp_dir();
        let support = tmp.path().join("support");
        let dir = verified_dir(tmp.path()).join("support").unwrap();
        let exe = support.join(APP).join(EXE);

        assert!(install(&dir, &app_tar(b"exe 1")).unwrap().fresh);
        assert_eq!(std::fs::read(&exe).unwrap(), b"exe 1");
        assert_eq!(
            std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!install(&dir, &app_tar(b"exe 1")).unwrap().fresh);

        let v2 = install(&dir, &app_tar(b"exe 2")).unwrap();
        assert!(v2.fresh);
        assert_eq!(v2.app, dir.join(APP).unwrap());
        assert_eq!(std::fs::read(&exe).unwrap(), b"exe 2");
        assert_eq!(
            std::fs::read_to_string(support.join(VERSION_FILE)).unwrap(),
            version_name(&app_tar(b"exe 2"))
        );
        assert_eq!(names(&support), [APP, LOCK_FILE, VERSION_FILE]);
    }

    /// Test that the install waits while another process holds the lock,
    /// so that two processes do not replace the app at the same time.
    ///   1. Hold the install lock
    ///   2. Start an install in a thread and check that it waits
    ///   3. Release the lock and check that the install completes
    #[test]
    fn install_waits_for_lock_of_other_install() {
        let tmp = temp_dir();
        let dir = verified_dir(tmp.path()).join("support").unwrap();
        let dir = data_dir::make_dir(&dir).unwrap();
        // A second open file description gets its own lock, as in another
        // process.
        let held = lock(&dir.join(LOCK_FILE).unwrap()).unwrap();
        let worker = {
            let dir = dir.clone();
            std::thread::spawn(move || install(&dir, &app_tar(b"exe")).unwrap())
        };
        std::thread::sleep(Duration::from_millis(300));
        assert!(!worker.is_finished());
        assert!(!tmp.path().join("support").join(APP).exists());
        drop(held);
        assert!(worker.join().unwrap().fresh);
        assert!(tmp.path().join("support").join(APP).join(EXE).is_file());
    }

    /// Test that the install refuses archives with special entries and an
    /// archive without the executable, and leaves no app. A link in the
    /// bundle can point out of the data directory.
    ///   1. Install an archive with a symlink in the bundle
    ///   2. Install an archive with a contiguous file in the bundle
    ///   3. Install an empty archive, as the placeholder of a build
    ///      without the app
    ///   4. Check the errors and that only the lock file stays
    #[test]
    fn install_refuses_links_and_incomplete_app_and_leaves_no_files() {
        let tmp = temp_dir();
        let dir = verified_dir(tmp.path()).join("support").unwrap();

        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        tar.append_link(&mut header, format!("{APP}/Contents"), "/etc")
            .unwrap();
        let err = install(&dir, &tar.into_inner().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not allowed"), "{err}");

        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Continuous);
        header.set_size(1);
        tar.append_data(&mut header, format!("{APP}/{EXE}"), &b"x"[..])
            .unwrap();
        let err = install(&dir, &tar.into_inner().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not allowed"), "{err}");

        let err = install(&dir, b"").unwrap_err().to_string();
        assert_eq!(err, msg::NOTIFIER_INCOMPLETE);
        assert_eq!(names(&tmp.path().join("support")), [LOCK_FILE]);
    }

    /// Test that the archive paths must stay in the app bundle.
    ///   1. Check clean paths with `.` and trailing slashes
    ///   2. Check that absolute paths, `..` and other top names are refused
    #[test]
    fn archive_paths_outside_app_bundle_are_refused() {
        assert_eq!(
            bundle_path("./AirlockNotifier.app/Contents/"),
            Some(format!("{APP}/Contents"))
        );
        assert_eq!(bundle_path("AirlockNotifier.app"), Some(APP.into()));
        assert_eq!(bundle_path("/AirlockNotifier.app/x"), None);
        assert_eq!(bundle_path("AirlockNotifier.app/../x"), None);
        assert_eq!(bundle_path("Other.app/x"), None);
    }

    /// Test that the cleanup selects only the temporary directories of
    /// stopped processes.
    ///   1. Give the app, the mark, temporary directories of a live and a
    ///      stopped process, and other names
    ///   2. Check the selection
    #[test]
    fn cleanup_selects_temp_dirs_of_stopped_processes() {
        let names = [APP, VERSION_FILE, ".tmp.10.0", ".tmp.20.3", ".tmp.x.1"].map(String::from);
        assert_eq!(stale_temps(names, |pid| pid == 10), [".tmp.20.3"]);
    }
}
