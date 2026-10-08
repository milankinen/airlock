use std::cell::RefCell;
use std::future::Future;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use airlock_test_utils::{TempDir, temp_dir};
use tokio::io::AsyncWriteExt;
use tokio::task::LocalSet;

use crate::bridge::make_fifo_at;

/// Run `fut` like airlockd does: on a current-thread runtime inside a
/// `LocalSet`. Shuts the runtime down without waiting for the blocking
/// FIFO opens of bridge loops that are still running.
pub(crate) fn run_bridge<F: Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let out = LocalSet::new().block_on(&rt, fut);
    rt.shutdown_background();
    out
}

/// A temp directory standing in for the container rootfs of a bridge.
/// On drop, opens each FIFO once so no blocked bridge thread is left.
pub(crate) struct BridgeDir {
    dir: TempDir,
    fifos: RefCell<Vec<PathBuf>>,
}

impl BridgeDir {
    pub fn new() -> Self {
        Self {
            dir: temp_dir(),
            fifos: RefCell::new(Vec::new()),
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Create FIFO `name` with the production helper, owned by us.
    pub fn fifo(&self, name: &str) -> PathBuf {
        let path = self.path(name);
        // SAFETY: getuid/getgid cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        make_fifo_at(&path, uid, gid).unwrap();
        self.fifos.borrow_mut().push(path.clone());
        path
    }

    /// Write shim `name` with `body`, its guest paths swapped for local
    /// ones.
    pub fn shim(&self, name: &str, body: &str, paths: &[(&str, &Path)]) -> PathBuf {
        let body = paths.iter().fold(body.to_string(), |b, (guest, local)| {
            b.replace(guest, local.to_str().unwrap())
        });
        let path = self.path(name);
        std::fs::write(&path, body).unwrap();
        path
    }
}

impl Drop for BridgeDir {
    fn drop(&mut self) {
        for fifo in self.fifos.borrow().iter() {
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(fifo);
        }
    }
}

/// Exit code and output of one shim run.
#[derive(Debug)]
pub(crate) struct ShimRun {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Run `shim` with `sh`, feeding it `stdin`, as a container process would.
pub(crate) async fn run_shim(shim: &Path, args: &[&str], stdin: &[u8]) -> ShimRun {
    let mut child = tokio::process::Command::new("sh")
        .arg(shim)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let stdin = stdin.to_vec();
    tokio::task::spawn_local(async move {
        let _ = input.write_all(&stdin).await;
    });
    let out = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap_or_else(|_| panic!("shim {} {args:?} hung", shim.display()))
        .unwrap();
    ShimRun {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Write `data` to `fifo` in one open-to-close cycle, like a raw writer.
pub(crate) async fn write_fifo(fifo: &Path, data: Vec<u8>) {
    let fifo = fifo.to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || std::fs::write(fifo, data)),
    )
    .await
    .expect("nobody read the fifo")
    .unwrap()
    .unwrap();
}

/// Wait until `cond` holds. Panics with `what` after five seconds.
pub(crate) async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
