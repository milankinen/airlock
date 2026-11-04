//! Tests for the sandbox directory: the sandbox lock, symlink
//! safety, the CA and the sandbox disk.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use crate::sandbox::disk::*;
use crate::sandbox::load::*;
use crate::sandbox::lock::{self, LockKind, SandboxBusy, SandboxLock, is_running};
use crate::sandbox::meta::read_run_meta;
use crate::test_cfg::{eventually, host_env_vault, temp_dir, test_context};
use crate::util::{PinnedDir, VerifiedPath};
use crate::vm::disk;

/// The permission bits of `path`.
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The verified form of the test path `path`.
fn verified(path: &Path) -> VerifiedPath {
    VerifiedPath::new_follow(path, None).unwrap()
}

/// Make the sandbox directory `<dir>/sandbox` of the project `dir`
/// and take its lock.
fn lock_sandbox(dir: &Path) -> anyhow::Result<SandboxLock> {
    let project = verified(dir);
    SandboxLock::acquire_dir(&project.join("sandbox")?, &project)
}

/// Test that only one holder can have the sandbox locks, and that the
/// running check follows both locks. A start must not take a sandbox
/// that another process starts or runs.
///   1. Lock the sandbox and check that it runs and that a second lock
///      fails as busy
///   2. Free the startup lock and check that the sandbox still runs
///   3. Free the run lock and check that the sandbox does not run
///   4. Lock the sandbox again
#[test]
fn sandbox_lock_is_exclusive_and_is_running_tracks_both_locks() {
    let tmp = temp_dir();
    let dir = verified(tmp.path()).join("sandbox").unwrap();
    let mut held = lock_sandbox(tmp.path()).unwrap();
    assert!(is_running(&dir));
    let busy = lock_sandbox(tmp.path()).err().unwrap();
    assert!(busy.is::<SandboxBusy>(), "{busy:#}");

    drop(held.take_startup());
    assert!(is_running(&dir));
    drop(held);
    // A fork in a parallel test can hold a copy of a file for a short
    // time.
    eventually("sandbox does not run", || (!is_running(&dir)).then_some(()));
    eventually("lock is taken", || lock_sandbox(tmp.path()).ok());
}

/// Test that the `.airlock` setup and the sandbox lock never write
/// through a symlink, so that a hostile project cannot make airlock
/// change host files.
///   1. Make `.airlock/.gitignore` and the lock file symlinks to a host
///      file
///   2. Set up `.airlock` and take the lock, and check that the host file
///      did not change
///   3. Remove the symlinks and check that the setup writes `.gitignore`
///      and the lock makes a regular lock file
///   4. Make `.airlock` a symlink to a directory and check the error,
///      and that the target stays empty
#[test]
fn cache_dir_and_lock_do_not_follow_symlinks() {
    let tmp = temp_dir();
    let dir = tmp.path().join("project");
    let victim = tmp.path().join("victim");
    std::fs::write(&victim, "host file").unwrap();
    std::fs::create_dir_all(dir.join(".airlock")).unwrap();
    std::fs::create_dir_all(dir.join("sandbox")).unwrap();
    symlink(&victim, dir.join(".airlock/.gitignore")).unwrap();
    symlink(&victim, dir.join("sandbox/run.lock")).unwrap();

    // The setup does not overwrite the `.gitignore` symlink. The lock
    // refuses its symlink.
    ensure_cache_dir(&verified(&dir)).unwrap();
    assert!(lock_sandbox(&dir).is_err());
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host file");

    std::fs::remove_file(dir.join(".airlock/.gitignore")).unwrap();
    std::fs::remove_file(dir.join("sandbox/run.lock")).unwrap();
    ensure_cache_dir(&verified(&dir)).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join(".airlock/.gitignore")).unwrap(),
        "*\n"
    );
    let lock = lock_sandbox(&dir).unwrap();
    let meta = std::fs::symlink_metadata(dir.join("sandbox/run.lock")).unwrap();
    assert!(meta.is_file());
    drop(lock);

    let other = tmp.path().join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::remove_dir_all(dir.join(".airlock")).unwrap();
    symlink(&other, dir.join(".airlock")).unwrap();
    let err = ensure_cache_dir(&verified(&dir)).unwrap_err();
    // The temp dir can be behind a symlink (macOS `/var`), so the
    // resolved path can differ from the given path.
    let real = std::fs::canonicalize(&dir).unwrap().join(".airlock");
    assert_eq!(
        err.to_string(),
        format!(
            "refusing path {} ({1}): {1} is a symlink",
            dir.join(".airlock").display(),
            real.display()
        )
    );
    assert!(!other.join(".gitignore").exists());
}

/// Test that the run data and the CA never go through a symlink in the
/// sandbox directory. A link must not leak a host file as the CA key or
/// let airlock write a host file.
///   1. Make `run.json`, `ca.json` and their temp files symlinks to
///      host files
///   2. Open the project
///   3. Check that the host files did not change, that the CA is new,
///      and that `run.json` and `ca.json` are regular files
#[test]
fn run_data_and_ca_do_not_follow_planted_symlinks() {
    let tmp = temp_dir();
    let dir = tmp.path().join("project");
    let sandbox = dir.join("sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();
    let victim = tmp.path().join("victim");
    let secret = tmp.path().join("secret");
    std::fs::write(&victim, "host file").unwrap();
    std::fs::write(&secret, r#"{"cert": "host", "key": "host"}"#).unwrap();
    symlink(&secret, sandbox.join("ca.json")).unwrap();
    symlink(&secret, sandbox.join("run.json")).unwrap();
    symlink(&victim, sandbox.join(".run.json.tmp")).unwrap();
    let ca_tmp = format!("ca.json.{}.tmp", std::process::id());
    symlink(&victim, sandbox.join(ca_tmp)).unwrap();

    let (project, _) = open_project(&dir);

    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host file");
    assert!(std::fs::read_to_string(&secret).unwrap().contains("host"));
    assert!(project.ca_cert.contains("BEGIN CERTIFICATE"));
    for name in ["ca.json", "run.json"] {
        let meta = std::fs::symlink_metadata(sandbox.join(name)).unwrap();
        assert!(meta.is_file(), "{name}");
    }
}

/// Make a FIFO at `path`.
fn mkfifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt as _;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

/// Test that a FIFO or a link at the disk path is no disk, and that the
/// check does not block on the FIFO.
///   1. Put a symlink at the disk path and check that there is no disk
///   2. Put a FIFO there and check that there is no disk
#[test]
fn planted_disk_link_or_fifo_is_no_disk() {
    let tmp = temp_dir();
    let (project, _) = open_project(tmp.path());
    let disk = project.sandbox_dir.to_path().unwrap().join(disk::DISK_FILE);
    let victim = tmp.path().join("victim");
    std::fs::write(&victim, "host file").unwrap();

    symlink(&victim, &disk).unwrap();
    assert!(!has_disk(&project.sandbox_dir));
    std::fs::remove_file(&disk).unwrap();
    mkfifo(&disk);
    assert!(!has_disk(&project.sandbox_dir));
}

/// Test that a FIFO at `ca.json` does not block the open and gets a
/// new CA, and that a directory there fails the open.
///   1. Put a FIFO at `ca.json` and open the project
///   2. Check that `ca.json` is a regular file with a new CA
///   3. Put a directory at `ca.json` and check that the open fails
#[test]
fn ca_json_fifo_is_replaced_and_ca_json_dir_fails_open() {
    let tmp = temp_dir();
    let dir = tmp.path().join("project");
    let sandbox = dir.join("sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();
    mkfifo(&sandbox.join("ca.json"));
    // Keep one lock for both opens. A fork in a parallel test can hold a
    // copy of a released lock for a short time, so a second acquire can
    // fail.
    let lock = lock_sandbox(&dir).unwrap();
    let open_with_lock = || {
        let config = crate::config::config_values::parse(serde_json::json!({})).unwrap();
        open(&lock, config, None, test_context(&dir, host_env_vault(&[])))
    };

    let project = open_with_lock().unwrap();
    assert!(project.ca_cert.contains("BEGIN CERTIFICATE"));
    assert!(
        std::fs::symlink_metadata(sandbox.join("ca.json"))
            .unwrap()
            .is_file()
    );
    drop(project);

    std::fs::remove_file(sandbox.join("ca.json")).unwrap();
    std::fs::create_dir(sandbox.join("ca.json")).unwrap();
    assert!(open_with_lock().is_err());
}

/// Test that an `.airlock` directory of another user is refused.
///   1. Check that a directory of the current user has no problem
///   2. Check the message for a directory of another user
#[test]
fn airlock_dir_owned_by_another_user_is_refused() {
    let path = Path::new("/some/.airlock");
    let euid = unsafe { libc::geteuid() };
    let other = euid.wrapping_add(1);
    let path = path.display();
    assert_eq!(airlock_dir_problem_for(&path, euid), None);
    assert_eq!(
        airlock_dir_problem_for(&path, other),
        Some("/some/.airlock is owned by another user".to_string())
    );
}

/// Open a project with an empty config in `dir`.
/// Returns:
///   The project, and the sandbox directory pinned for disk resets.
fn open_project(dir: &Path) -> (Project, PinnedDir) {
    let sandbox_lock = lock_sandbox(dir).unwrap();
    let config = crate::config::config_values::parse(serde_json::json!({})).unwrap();
    let project = open(
        &sandbox_lock,
        config,
        None,
        test_context(dir, host_env_vault(&[])),
    )
    .unwrap();
    let pinned = PinnedDir::pin(project.sandbox_dir.to_path().unwrap()).unwrap();
    (project, pinned)
}

/// Test that an opened project has a private sandbox directory and CA,
/// and records its guest working directory. Other users must not read
/// the CA key.
///   1. Open a project
///   2. Check the modes of the sandbox directory and the CA file, and
///      the CA certificate
///   3. Save the run metadata and check the guest working directory
#[test]
fn opened_project_has_private_ca_and_run_metadata() {
    let tmp = temp_dir();
    let (project, _) = open_project(tmp.path());
    let dir = project.sandbox_dir.to_path().unwrap();
    assert_eq!(mode(dir), 0o700);
    assert_eq!(mode(&dir.join("ca.json")), 0o600);
    assert!(project.ca_cert.contains("BEGIN CERTIFICATE"));
    project.save_meta();
    assert!(read_run_meta(&project.sandbox_dir).guest_cwd.is_some());
}

/// Test that the disk identity stays the same while the disk stays, and
/// that a reset removes the disk and its identity. The install records
/// use the identity to find out if the disk is the same.
///   1. Check that a new project has no disk and no identity
///   2. Create the disk twice and check that the identity stays
///   3. Reset the disk and check that the disk and its identity are gone
///   4. Reset again with no disk and check that it succeeds
#[test]
fn disk_identity_stays_until_reset() {
    let tmp = temp_dir();
    let (project, pinned) = open_project(tmp.path());
    let path = project.sandbox_dir.to_path().unwrap().to_path_buf();
    let dir = &path;
    assert!(disk_id(&pinned).is_none());
    assert!(!has_disk(&project.sandbox_dir));

    ensure_disk(&pinned, &project.config.disk, false).unwrap();
    assert!(has_disk(&project.sandbox_dir));
    let id = disk_id(&pinned).unwrap();
    ensure_disk(&pinned, &project.config.disk, false).unwrap();
    assert_eq!(disk_id(&pinned), Some(id));

    reset_disk(&pinned).unwrap();
    assert!(disk_id(&pinned).is_none());
    assert!(!has_disk(&project.sandbox_dir));
    assert!(!dir.join(disk::DISK_ID_FILE).exists());
    reset_disk(&pinned).unwrap();
}

/// Test that each new disk gets a new identity, and that the identity
/// belongs to the disk file. A new disk must not look like the old disk.
///   1. Make a disk with the sandbox prepare, reset it, and make a disk
///      again, and check that the identity changed
///   2. Remove the disk file and check that there is no identity
///   3. Make a disk and check that it has a new identity
///   4. Remove the identity file and check that a new disk gets one
#[test]
fn each_new_disk_gets_new_identity() {
    let tmp = temp_dir();
    let (project, pinned) = open_project(tmp.path());
    let path = project.sandbox_dir.to_path().unwrap().to_path_buf();
    let dir = &path;
    let prepare = || ensure_disk(&pinned, &project.config.disk, false).unwrap();

    disk::prepare(&project.sandbox_dir, &project.config.disk, "/root", "/").unwrap();
    let id = disk_id(&pinned).unwrap();
    reset_disk(&pinned).unwrap();
    prepare();
    let id2 = disk_id(&pinned).unwrap();
    assert_ne!(id2, id);

    std::fs::remove_file(dir.join(disk::DISK_FILE)).unwrap();
    assert!(disk_id(&pinned).is_none());
    prepare();
    assert_ne!(disk_id(&pinned), Some(id2));

    std::fs::remove_file(dir.join(disk::DISK_ID_FILE)).unwrap();
    assert!(disk_id(&pinned).is_none());
    prepare();
    assert!(disk_id(&pinned).is_some());
}

/// Test that a disk reset never follows a symlink. A link at the disk
/// path must not let a reset change a host file.
///   1. Replace the disk with a symlink to a host file
///   2. Reset the disk
///   3. Check that the link is gone and the host file did not change
#[test]
fn disk_reset_does_not_follow_symlink() {
    let tmp = temp_dir();
    let (project, pinned) = open_project(tmp.path());
    let path = project.sandbox_dir.to_path().unwrap().to_path_buf();
    let dir = &path;
    ensure_disk(&pinned, &project.config.disk, false).unwrap();

    let victim = tmp.path().join("victim");
    std::fs::write(&victim, "host file").unwrap();
    std::fs::remove_file(dir.join(disk::DISK_FILE)).unwrap();
    symlink(&victim, dir.join(disk::DISK_FILE)).unwrap();
    reset_disk(&pinned).unwrap();
    assert!(!has_disk(&project.sandbox_dir));
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host file");
}

/// Test that the sandbox process takes over only the lock files of its
/// own sandbox. A descriptor of another file must not count as a lock.
///   1. Take both locks, then give the descriptor of another file as
///      the run lock
///   2. Check that the take-over is refused and the file stays open
///   3. Give the descriptors of both locks
///   4. Check that the take-over holds both locks, writes the PID and
///      makes the descriptors close on exec
#[test]
fn adopt_refuses_fd_of_other_file_and_keeps_own_locks() {
    use std::os::unix::io::{AsRawFd, IntoRawFd};

    let tmp = temp_dir();
    let project = verified(tmp.path());
    let held = lock_sandbox(tmp.path()).unwrap();
    let dir = held.dir().clone();
    let SandboxLock {
        _run: run, startup, ..
    } = held;
    let startup = startup.unwrap();
    let other = tmp.path().join("other");
    std::fs::write(&other, b"").unwrap();
    let other = std::fs::File::open(&other).unwrap();
    let adopted = SandboxLock::adopt(
        other.as_raw_fd(),
        startup.as_raw_fd(),
        project.clone(),
        dir.clone(),
    );
    assert!(adopted.is_err());
    // The refusal does not close the unrelated file.
    assert!(other.metadata().is_ok());
    drop(other);

    let run_fd = run.file.into_raw_fd();
    let _lock =
        SandboxLock::adopt(run_fd, startup.file.into_raw_fd(), project, dir.clone()).unwrap();
    assert!(lock::is_held(&dir, LockKind::Run));
    assert!(lock::is_held(&dir, LockKind::Startup));
    let pid = std::fs::read_to_string(tmp.path().join("sandbox/run.lock")).unwrap();
    assert_eq!(pid, std::process::id().to_string());
    // The lock owns the descriptor now.
    let flags = unsafe { libc::fcntl(run_fd, libc::F_GETFD) };
    assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
}
