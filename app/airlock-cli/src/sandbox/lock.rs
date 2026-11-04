//! Sandbox locks.
//!
//! Two lock files in the sandbox directory control the use of a sandbox:
//!  * The run lock is held by the sandbox process from its start to its
//!    exit. Thus a held run lock tells that a sandbox process is alive: it
//!    boots, runs or stops. The file holds the process ID of the sandbox
//!    process.
//!  * The startup lock lets only one process at a time start a sandbox
//!    process. The process that starts it holds the lock while it decides.
//!    The sandbox process then holds it until its parent connected.
//!
//! A process can give a held lock to its child process, and the child
//! checks that it got the correct lock. The kernel frees a lock when the
//! last holder exits, also after a crash. A wait for a lock stops on a
//! Ctrl+C and tells the user after a short time that it waits.

use std::fmt::Display;
use std::fs::File;
use std::future::Future;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::sandbox::Interrupted;
use crate::util::{PinnedDir, VerifiedPath};
use crate::{cli, messages};

/// Wait time before a lock wait tells the user that it waits.
pub const WAITING_NOTICE_DELAY: Duration = Duration::from_secs(1);

/// Size limit of a lock file. It contains only a PID.
const LOCK_FILE_CAP: u64 = 64;

/// The two lock files of a sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockKind {
    /// `run.lock`: a sandbox process is alive.
    Run,
    /// `startup.lock`: a process starts the sandbox process.
    Startup,
}

impl LockKind {
    /// Get the name of the lock file in the sandbox directory.
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Run => "run.lock",
            Self::Startup => "startup.lock",
        }
    }
}

/// One held lock file. The drop frees the lock, unless a child process
/// got a copy of the open file.
#[derive(Debug)]
pub struct HeldLock {
    kind: LockKind,
    pub(super) file: File,
}

impl AsRawFd for HeldLock {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

/// State of a lock, as found by [`probe`].
pub enum IdleLock {
    /// The lock file does not exist.
    Missing,
    /// No process held the lock. This file holds it until it drops.
    Held(File),
    /// A different process holds the lock.
    Busy,
}

/// Take the lock `kind` of `sandbox_dir` if no process holds it. Makes the
/// lock file if it does not exist. Does not wait.
/// Returns:
///   The held lock, or `None` if a different process holds it. Error if
///   the directory or the file cannot open, or the file is not a regular
///   file.
pub(crate) fn try_lock(
    sandbox_dir: &VerifiedPath,
    kind: LockKind,
) -> anyhow::Result<Option<HeldLock>> {
    let dir = PinnedDir::pin(sandbox_dir.to_path()?)?;
    try_lock_pinned(&dir, kind)
}

/// Take the lock `kind` in the pinned sandbox directory `dir` (see
/// [`try_lock`]).
pub(super) fn try_lock_pinned(dir: &PinnedDir, kind: LockKind) -> anyhow::Result<Option<HeldLock>> {
    let file = open_lock_file(dir, kind)?;
    if !flock(&file, false)? {
        return Ok(None);
    }
    let held = HeldLock { kind, file };
    clear_pid(&held)?;
    Ok(Some(held))
}

/// Wait until this process holds the lock `kind` of `sandbox_dir`. Makes
/// the lock file if it does not exist. A Ctrl+C or SIGTERM ends the wait.
/// Args:
///  - `sandbox_dir`: Sandbox directory with the lock file
///  - `kind`: The lock to take
///  - `notice`: Called one time if the wait takes longer than
///    [`WAITING_NOTICE_DELAY`]
///
/// Returns:
///   The held lock. Error [`Interrupted`] after a Ctrl+C or SIGTERM, or
///   error if the file cannot open or lock.
pub async fn wait_lock(
    sandbox_dir: &VerifiedPath,
    kind: LockKind,
    notice: impl FnOnce(),
) -> anyhow::Result<HeldLock> {
    wait_lock_until(sandbox_dir, kind, notice, cli::interrupted())
        .await?
        .ok_or_else(|| Interrupted.into())
}

/// Wait until this process holds the lock `kind` of `sandbox_dir`, or
/// until `cancel` completes (see [`wait_lock`]).
/// Returns:
///   The held lock, or `None` if `cancel` completed first.
// The wait is a blocking `flock` in its own thread. Thus it does not poll,
// and it gets the lock at once when the holder frees it. After a cancel, the
// thread can still get the lock. It then sends the file to a closed
// channel, and the drop of the file frees the lock again. A plain thread,
// not a blocking task of the runtime: the runtime waits for its blocking
// tasks at the exit of the process.
pub(crate) async fn wait_lock_until(
    sandbox_dir: &VerifiedPath,
    kind: LockKind,
    notice: impl FnOnce(),
    cancel: impl Future<Output = ()>,
) -> anyhow::Result<Option<HeldLock>> {
    let dir = PinnedDir::pin(sandbox_dir.to_path()?)?;
    let file = open_lock_file(&dir, kind)?;
    let Some(file) = wait_file(file, notice, cancel).await? else {
        return Ok(None);
    };
    let held = HeldLock { kind, file };
    clear_pid(&held)?;
    Ok(Some(held))
}

/// Wait until no process holds the lock `kind` of `sandbox_dir`, or until
/// `cancel` completes. Makes nothing, and does not keep the lock.
/// Returns:
///   `true` when the lock is free or its file does not exist, `false` if
///   `cancel` completed first. Error if the wait fails.
pub async fn wait_free_until(
    sandbox_dir: &VerifiedPath,
    kind: LockKind,
    cancel: impl Future<Output = ()>,
) -> anyhow::Result<bool> {
    let file = match probe(sandbox_dir, kind) {
        IdleLock::Missing | IdleLock::Held(_) => return Ok(true),
        IdleLock::Busy => open_existing(sandbox_dir, kind),
    };
    // The holder can free the lock and remove the sandbox between the two
    // steps.
    let Ok(file) = file else {
        return Ok(true);
    };
    Ok(wait_file(file, || {}, cancel).await?.is_some())
}

/// Wait until `file` holds its lock, or until `cancel` completes (see
/// [`wait_lock_until`]).
/// Returns:
///   The file that holds the lock, or `None` if `cancel` completed first.
// The wait is a blocking `flock` in its own thread. Thus it does not poll,
// and it gets the lock at once when the holder frees it. After a cancel, the
// thread can still get the lock. It then sends the file to a closed
// channel, and the drop of the file frees the lock again. A plain thread,
// not a blocking task of the runtime: the runtime waits for its blocking
// tasks at the exit of the process.
async fn wait_file(
    file: File,
    notice: impl FnOnce(),
    cancel: impl Future<Output = ()>,
) -> anyhow::Result<Option<File>> {
    if flock(&file, false)? {
        return Ok(Some(file));
    }
    let (tx, rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("lock wait".into())
        .spawn(move || {
            let result = flock(&file, true).map(|_| file);
            let _ = tx.send(result);
        })?;
    let mut rx = std::pin::pin!(rx);
    let mut cancel = std::pin::pin!(cancel);
    let timer = tokio::time::sleep(WAITING_NOTICE_DELAY);
    let mut timer = std::pin::pin!(timer);
    let mut notice = Some(notice);
    loop {
        tokio::select! {
            result = &mut rx => return Ok(Some(result??)),
            () = &mut timer, if notice.is_some() => {
                if let Some(notice) = notice.take() {
                    notice();
                }
            }
            () = &mut cancel => return Ok(None),
        }
    }
}

/// Open the existing lock file `kind` of `sandbox_dir` to read. Does not
/// follow a symlink, and refuses a file that is not a regular file.
fn open_existing(sandbox_dir: &VerifiedPath, kind: LockKind) -> anyhow::Result<File> {
    let dir = PinnedDir::pin(sandbox_dir.to_path()?)?;
    dir.open_regular(kind.file_name())?
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound).into())
}

/// Check the lock `kind` of `sandbox_dir`. Unlike [`try_lock`], it makes
/// nothing.
/// Args:
///  - `sandbox_dir`: Sandbox directory. Not followed if it is a symlink
///
/// Returns:
///   State of the lock, see [`IdleLock`].
pub fn probe(sandbox_dir: &VerifiedPath, kind: LockKind) -> IdleLock {
    use std::os::unix::fs::OpenOptionsExt;

    // Do not follow a symlink at `sandbox_dir` or at the lock file. A
    // symlink must not give access to the lock of a different sandbox.
    // airlock never makes a symlinked `sandbox_dir`, so treat it as
    // `Missing`.
    if sandbox_dir
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return IdleLock::Missing;
    }
    // O_NONBLOCK: the open of a planted FIFO must not block. The
    // regular-file check below refuses the FIFO.
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let opened = sandbox_dir
        .join(kind.file_name())
        .and_then(|lock| lock.open_with(&opts));
    let Ok(file) = opened else {
        return IdleLock::Missing;
    };
    match file.metadata() {
        Ok(meta) if meta.is_file() => {}
        _ => return IdleLock::Missing,
    }
    // A lock check, not `kill(pid, 0)`: no problems with PID reuse or with
    // the process of a different user.
    match flock(&file, false) {
        Ok(true) => IdleLock::Held(file),
        _ => IdleLock::Busy,
    }
}

/// Check if a process holds the lock `kind` of `sandbox_dir`.
pub fn is_held(sandbox_dir: &VerifiedPath, kind: LockKind) -> bool {
    matches!(probe(sandbox_dir, kind), IdleLock::Busy)
}

/// Take over the lock `kind` that the parent process holds and gives to
/// this process as the open descriptor `fd`. If the lock is free, this
/// takes it. The descriptor does not go to the child processes of this
/// process. The run lock file gets the PID of this process.
/// Args:
///  - `fd`: Inherited descriptor of the lock file. The value takes it
///  - `sandbox_dir`: Sandbox directory with the lock file
///  - `kind`: The lock that `fd` must hold
///
/// Returns:
///   The held lock, or error if `fd` is not open or is not the lock file
///   of `sandbox_dir`, or if a different holder has the lock.
pub fn adopt_lock(
    fd: RawFd,
    sandbox_dir: &VerifiedPath,
    kind: LockKind,
) -> anyhow::Result<HeldLock> {
    // Refuse the standard streams: this process uses them.
    // SAFETY: F_GETFD only reads the descriptor flags.
    if fd <= 2 || unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        anyhow::bail!(messages::sandbox_process::LOCK_FD_MISMATCH);
    }
    // Do not own the descriptor before the check below. Thus a wrong
    // descriptor number does not close an unrelated file of this process.
    // SAFETY: The descriptor is open (see above). The value never closes
    // it.
    let borrowed = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    // The descriptor must be the lock file of this sandbox. Compare the
    // file identity with the entry in the sandbox directory, which is not
    // followed. Thus a descriptor of another file, or a symlink at the
    // lock file, is refused.
    let entry = sandbox_dir.join(kind.file_name())?.symlink_metadata()?;
    let held = borrowed.metadata()?;
    if !held.is_file() || held.dev() != entry.dev() || held.ino() != entry.ino() {
        anyhow::bail!(messages::sandbox_process::LOCK_FD_MISMATCH);
    }
    let file = std::mem::ManuallyDrop::into_inner(borrowed);
    // The VM helper processes must not hold the lock after this process
    // ends.
    // SAFETY: `file` owns the open descriptor.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    // The dev/ino check above is the guard. This call keeps a lock that the
    // parent holds on the same open file, but it does not prove that the
    // parent held it: on a free lock it simply takes the lock. It fails if
    // a different holder has the lock.
    if !flock(&file, false)? {
        anyhow::bail!(messages::project::SANDBOX_IN_USE);
    }
    let held = HeldLock { kind, file };
    write_pid(&held)?;
    Ok(held)
}

/// Read the PID in the run lock file of the sandbox `dir`.
/// Returns:
///   The PID text, or `None` if the file has none.
pub(super) fn read_holder(dir: &PinnedDir) -> Option<String> {
    dir.read(LockKind::Run.file_name(), LOCK_FILE_CAP)
        .ok()
        .flatten()
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Open the lock file `kind` in `dir`. Makes it if it does not exist.
fn open_lock_file(dir: &PinnedDir, kind: LockKind) -> anyhow::Result<File> {
    // The open does not follow a symlink, and it refuses a file that is not
    // a regular file. Append mode: the PID write goes to the start after
    // the truncate.
    Ok(dir.open_append(kind.file_name(), 0o644)?)
}

/// Take an exclusive `flock` on `file`.
/// Args:
///  - `block`: Wait until the lock is free
///
/// Returns:
///   `true` if this file holds the lock, `false` if a different holder has
///   it and `block` is `false`. Error for other failures.
fn flock(file: &File, block: bool) -> std::io::Result<bool> {
    let op = if block {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    loop {
        // SAFETY: the descriptor is open. `flock` does not touch memory.
        if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::EWOULDBLOCK) if !block => return Ok(false),
            _ => {
                return Err(std::io::Error::other(messages::project::lock_failed(err)));
            }
        }
    }
}

/// Remove the PID of an old holder from the held run lock `held`. Only the
/// sandbox process writes its PID (see [`adopt_lock`]). Thus the
/// diagnostics never show a dead process or the process of a client. Does
/// nothing for the startup lock.
fn clear_pid(held: &HeldLock) -> anyhow::Result<()> {
    if held.kind == LockKind::Run {
        held.file.set_len(0)?;
    }
    Ok(())
}

/// Write the PID of this process to the held run lock `held`, for the
/// diagnostics. Does nothing for the startup lock.
fn write_pid(held: &HeldLock) -> anyhow::Result<()> {
    if held.kind != LockKind::Run {
        return Ok(());
    }
    let mut file = &held.file;
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(())
}

/// Held locks of the sandbox of one project: the run lock and, until the
/// start ends, the startup lock (see the module docs).
///
/// Taking the locks creates the sandbox directory. The locks are released
/// when the value drops or the process exits. [`Project`] holds no lock,
/// so the caller keeps this value while the sandbox must stay exclusive
/// (usually the whole run, see [`super::load::open`]). A second lock in the
/// same process fails like a lock from a different process.
pub struct SandboxLock {
    host_cwd: VerifiedPath,
    sandbox_dir: VerifiedPath,
    /// Held run lock. Never read. It is the RAII guard that releases the
    /// lock when `SandboxLock` drops.
    pub(super) _run: HeldLock,
    /// Held startup lock, until [`Self::take_startup`].
    pub(super) startup: Option<HeldLock>,
}

impl SandboxLock {
    /// Create the sandbox `id` in the data directory and take its locks.
    /// Fails when a different airlock instance holds a lock. Tests use it
    /// for the locks that a sandbox process holds.
    /// Args:
    ///  - `host_cwd`: Canonical project directory
    ///  - `boxes_dir`: Sandboxes directory in the data directory
    ///  - `id`: Registry id of the sandbox
    #[cfg(test)]
    pub fn acquire_box(
        host_cwd: &VerifiedPath,
        boxes_dir: &VerifiedPath,
        id: &str,
    ) -> anyhow::Result<Self> {
        let (sandbox_dir, pinned) = open_box_dir(boxes_dir, id)?;
        Self::lock(sandbox_dir, host_cwd.clone(), &pinned)
    }

    /// Create the sandbox directory `sandbox_dir` that has no project, and
    /// take its locks. A template build uses this. Fails when a different
    /// airlock instance holds a lock.
    /// Args:
    ///  - `sandbox_dir`: Sandbox directory, created with mode 0700
    ///  - `host_cwd`: Directory that the sandbox uses as the project
    ///    directory
    pub fn acquire_dir(
        sandbox_dir: &VerifiedPath,
        host_cwd: &VerifiedPath,
    ) -> anyhow::Result<Self> {
        let sandbox_dir = crate::data_dir::create_private_dir(sandbox_dir)?;
        let pinned = PinnedDir::pin(sandbox_dir.to_path()?)?;
        Self::lock(sandbox_dir, host_cwd.clone(), &pinned)
    }

    /// Take the startup lock and the run lock of the sandbox directory
    /// `pinned`, in this order. All lock users take them in this order.
    fn lock(
        sandbox_dir: VerifiedPath,
        host_cwd: VerifiedPath,
        pinned: &PinnedDir,
    ) -> anyhow::Result<Self> {
        // The sandbox holds the CA private key. Keep other local users out.
        // This is defense in depth only. The key file is always 0600.
        harden_dir_permissions(&sandbox_dir);
        let busy = || SandboxBusy {
            pid: read_holder(pinned),
        };
        let startup = try_lock_retry(pinned, LockKind::Startup)?.ok_or_else(busy)?;
        let run = try_lock_retry(pinned, LockKind::Run)?.ok_or_else(busy)?;
        Ok(Self {
            host_cwd,
            sandbox_dir,
            _run: run,
            startup: Some(startup),
        })
    }

    /// Take over the sandbox locks that the parent process holds and gives
    /// to this process as open descriptors (see [`adopt_lock`]). The
    /// sandbox process uses this.
    /// Args:
    ///  - `run_fd`: Inherited descriptor of the run lock file
    ///  - `startup_fd`: Inherited descriptor of the startup lock file
    ///  - `host_cwd`: Canonical project directory
    ///  - `sandbox_dir`: Sandbox directory with the lock files
    ///
    /// Returns:
    ///   The held locks, or error if a descriptor is not the held lock file
    ///   of its kind.
    pub fn adopt(
        run_fd: std::os::unix::io::RawFd,
        startup_fd: std::os::unix::io::RawFd,
        host_cwd: VerifiedPath,
        sandbox_dir: VerifiedPath,
    ) -> anyhow::Result<Self> {
        let run = adopt_lock(run_fd, &sandbox_dir, LockKind::Run)?;
        let startup = adopt_lock(startup_fd, &sandbox_dir, LockKind::Startup)?;
        Ok(Self {
            host_cwd,
            sandbox_dir,
            _run: run,
            startup: Some(startup),
        })
    }

    /// Get the sandbox directory.
    pub fn dir(&self) -> &VerifiedPath {
        &self.sandbox_dir
    }

    /// Get the canonical project directory of the sandbox.
    pub fn host_cwd(&self) -> &VerifiedPath {
        &self.host_cwd
    }

    /// Take the held startup lock out of the value. The run lock stays.
    /// The drop of the returned lock lets other processes connect to the
    /// sandbox, but not start a second sandbox process.
    pub fn take_startup(&mut self) -> Option<HeldLock> {
        self.startup.take()
    }
}

/// Create the directory of the sandbox `id` in the data directory, if it
/// does not exist. Takes no lock.
/// Args:
///  - `boxes_dir`: Sandboxes directory in the data directory
///  - `id`: Registry id of the sandbox
///
/// Returns:
///   The sandbox directory, or error if `id` is not an id or the directory
///   cannot be made.
pub fn create_box_dir(boxes_dir: &VerifiedPath, id: &str) -> anyhow::Result<VerifiedPath> {
    let (sandbox_dir, _) = open_box_dir(boxes_dir, id)?;
    harden_dir_permissions(&sandbox_dir);
    Ok(sandbox_dir)
}

/// Create and pin the directory of the sandbox `id` in `boxes_dir` (see
/// [`create_box_dir`]).
fn open_box_dir(boxes_dir: &VerifiedPath, id: &str) -> anyhow::Result<(VerifiedPath, PinnedDir)> {
    anyhow::ensure!(
        crate::sandbox::registry::is_valid_id(id),
        messages::sandbox::not_sandbox_id(id)
    );
    let boxes_dir = crate::data_dir::create_private_dir(boxes_dir)?;
    let pinned = PinnedDir::open(boxes_dir.to_path()?, id.as_ref(), true)?;
    Ok((boxes_dir.join(id)?, pinned))
}

/// Number of tries of a lock take in [`try_lock_retry`].
const LOCK_TRIES: u32 = 10;

/// Wait time between two tries of a lock take.
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);

/// Take the lock `kind` in the pinned sandbox directory `dir`, with a few
/// short tries.
/// Returns:
///   The held lock, or `None` if a different process holds it after all
///   tries.
// A probe of the lock state (for example `airlock info`, or the running
// check of a client) takes a free lock for a moment. Thus one failed try
// can be a probe and not a holder. The tries keep `airlock start` from a
// false "in use" error. The waits of the clients block, so a probe does not
// affect them.
fn try_lock_retry(dir: &PinnedDir, kind: LockKind) -> anyhow::Result<Option<HeldLock>> {
    for attempt in 1..=LOCK_TRIES {
        if let Some(held) = try_lock_pinned(dir, kind)? {
            return Ok(Some(held));
        }
        if attempt < LOCK_TRIES {
            std::thread::sleep(LOCK_RETRY_DELAY);
        }
    }
    Ok(None)
}

/// Check if a project runs now: a process holds its run lock or its
/// startup lock (see the module docs).
pub fn is_running(sandbox_dir: &VerifiedPath) -> bool {
    is_held(sandbox_dir, LockKind::Run) || is_held(sandbox_dir, LockKind::Startup)
}

/// Error when a different instance holds the sandbox lock.
#[derive(Debug)]
pub struct SandboxBusy {
    /// PID of the holder from the lock file, if the file has one.
    pub pid: Option<String>,
}

impl Display for SandboxBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.pid {
            Some(pid) => f.write_str(&messages::project::sandbox_in_use_by_pid(pid)),
            None => f.write_str(messages::project::SANDBOX_IN_USE),
        }
    }
}

impl std::error::Error for SandboxBusy {}

/// Try to limit a directory to its owner (0700). Ignores failure (for
/// example on filesystems without Unix modes). The sensitive file in it is
/// always 0600, which is the real protection.
fn harden_dir_permissions(dir: &VerifiedPath) {
    use std::os::unix::fs::PermissionsExt;
    let _ = dir.set_permissions(std::fs::Permissions::from_mode(0o700));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::test_cfg::cli_server::STILL_WAITS;
    use crate::test_cfg::{block_on_local, eventually, temp_dir};

    /// The verified form of the directory `path`.
    fn verified(path: &std::path::Path) -> VerifiedPath {
        VerifiedPath::new_follow(path, None).unwrap()
    }

    /// Test that a lock wait blocks while a different holder has the lock,
    /// tells the user that it waits, and gets the lock at once when the
    /// holder frees it. Also that the try does not wait.
    ///   1. Take the run lock and check that a try fails and the probe
    ///      says busy
    ///   2. Start a wait and check that it waits and calls the notice
    ///   3. Free the lock and check that the wait gets it
    ///   4. Check that the run lock file has no PID: only the sandbox process
    ///      writes it
    #[test]
    fn lock_wait_blocks_until_holder_frees_lock() {
        let tmp = temp_dir();
        let dir = verified(tmp.path());
        block_on_local(async {
            let held = try_lock(&dir, LockKind::Run).unwrap().unwrap();
            assert!(try_lock(&dir, LockKind::Run).unwrap().is_none());
            assert!(is_held(&dir, LockKind::Run));

            let noticed = std::cell::Cell::new(false);
            let wait = wait_lock_until(
                &dir,
                LockKind::Run,
                || noticed.set(true),
                std::future::pending(),
            );
            let mut wait = std::pin::pin!(wait);
            tokio::select! {
                r = &mut wait => panic!("wait got a held lock: {r:?}"),
                () = tokio::time::sleep(WAITING_NOTICE_DELAY + STILL_WAITS) => {}
            }
            assert!(noticed.get());

            drop(held);
            let got = tokio::time::timeout(Duration::from_secs(5), wait)
                .await
                .expect("wait did not get the free lock")
                .unwrap()
                .unwrap();
            assert_eq!(got.kind, LockKind::Run);
        });
        // Only the sandbox process writes its PID.
        assert!(
            std::fs::read_to_string(tmp.path().join("run.lock"))
                .unwrap()
                .is_empty()
        );
    }

    /// Test that a cancel ends a lock wait, and that the wait thread does
    /// not keep the lock when it gets it later. A Ctrl+C must not leave a
    /// held lock behind.
    ///   1. Take the startup lock, and start a wait that is canceled
    ///   2. Check that the wait returns no lock
    ///   3. Free the lock and check that a try gets it
    #[test]
    fn canceled_lock_wait_leaves_lock_free() {
        let tmp = temp_dir();
        let dir = verified(tmp.path());
        block_on_local(async {
            let held = try_lock(&dir, LockKind::Startup).unwrap().unwrap();
            let cancel = tokio::time::sleep(Duration::from_millis(50));
            let got = wait_lock_until(&dir, LockKind::Startup, || {}, cancel)
                .await
                .unwrap();
            assert!(got.is_none());
            drop(held);
        });
        // A fork in a parallel test can hold a copy of the file for a short
        // time.
        eventually("lock is free", || {
            try_lock(&dir, LockKind::Startup).unwrap()
        });
        assert!(
            std::fs::read_to_string(tmp.path().join("startup.lock"))
                .unwrap()
                .is_empty()
        );
    }

    /// Test that a child gets the held lock as a descriptor and takes it
    /// over, and that a wrong descriptor or kind is refused. A wrong
    /// descriptor must not give the sandbox to a process that does not
    /// hold its lock.
    ///   1. Take both locks of a sandbox
    ///   2. Check that the run lock descriptor is refused as the startup
    ///      lock, and a descriptor of another file is refused
    ///   3. Adopt both descriptors and check that both stay held
    #[test]
    fn adopt_accepts_only_held_lock_of_its_kind() {
        use std::os::unix::io::IntoRawFd;

        let tmp = temp_dir();
        let dir = verified(tmp.path());
        let run = try_lock(&dir, LockKind::Run).unwrap().unwrap();
        let startup = try_lock(&dir, LockKind::Startup).unwrap().unwrap();
        let other = File::open("/dev/null").unwrap();

        assert!(adopt_lock(run.as_raw_fd(), &dir, LockKind::Startup).is_err());
        assert!(adopt_lock(other.as_raw_fd(), &dir, LockKind::Run).is_err());

        let run = adopt_lock(run.file.into_raw_fd(), &dir, LockKind::Run).unwrap();
        let startup = adopt_lock(startup.file.into_raw_fd(), &dir, LockKind::Startup).unwrap();
        assert!(is_held(&dir, LockKind::Run));
        assert!(is_held(&dir, LockKind::Startup));
        drop((run, startup));
    }
}
