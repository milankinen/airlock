//! Escape tests for the Seatbelt sandbox of the libkrun VM process.
//!
//! The sandbox cannot be removed, so the checks run in a child process.

use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::seatbelt::{Policy, apply};
use crate::test_cfg::temp_dir;

/// Test root dir. Its presence makes the test run as the child.
const CHILD_ENV: &str = "AIRLOCK_TEST_SEATBELT_ROOT";

/// Test that the sandbox keeps the VM process in its shares and sockets.
/// libkrun has no checks of its own.
///   1. Make a read-only share, a file outside it, links in the share to
///      the outside file and a socket dir
///   2. Start the test binary again as a child that applies the policy
///   3. In the child, check that share reads and the listed socket bind
///      work, and that outside reads (direct, `..`, links, `/.vol`), share
///      writes, TCP connects and other socket binds are denied
#[test]
fn vm_process_sandbox_denies_escapes_from_shares_and_sockets() {
    if let Ok(root) = std::env::var(CHILD_ENV) {
        std::process::exit(child(Path::new(&root)));
    }

    let tmp = temp_dir();
    // The temp dir on macOS is below the `/var` link.
    let root = &std::fs::canonicalize(tmp.path()).unwrap();
    let share = root.join("share");
    std::fs::create_dir_all(share.join("sub")).unwrap();
    std::fs::create_dir(root.join("sock")).unwrap();
    std::fs::write(share.join("inside.txt"), "inside").unwrap();
    std::fs::write(root.join("outside.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(root.join("outside.txt"), share.join("abs")).unwrap();
    std::os::unix::fs::symlink("../outside.txt", share.join("rel")).unwrap();

    let module = module_path!().split_once("::").unwrap().1;
    let name = format!("{module}::vm_process_sandbox_denies_escapes_from_shares_and_sockets");
    let out = Command::new(std::env::current_exe().unwrap())
        .args([name.as_str(), "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, root)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "child failed:\n{text}");
    assert!(
        text.contains("all checks done"),
        "child did not run:\n{text}"
    );
}

/// Run the checks in the child.
/// Returns:
///   Exit code, 0 if all checks pass.
fn child(root: &Path) -> i32 {
    let share = root.join("share");
    let outside = root.join("outside.txt");
    let socket_ok = root.join("sock/ok.sock");
    let socket_other = root.join("sock/other.sock");
    let vol = |path: &Path| {
        let meta = std::fs::metadata(path).unwrap();
        PathBuf::from(format!("/.vol/{}/{}", meta.dev(), meta.ino()))
    };
    let vol_inside = vol(&share.join("inside.txt"));
    let vol_outside = vol(&outside);
    let vol_share_parent = vol(&share).join("..");
    // Listener from before the apply, so a failed connect is a sandbox deny.
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let tcp_addr = tcp.local_addr().unwrap();

    apply(&Policy {
        read_only: vec![share.to_str().unwrap().to_string()],
        unix_sockets: vec![socket_ok.to_str().unwrap().to_string()],
        ..Policy::default()
    })
    .unwrap();

    let mut failures = Vec::new();
    let mut allowed = |what: &str, result: std::io::Result<()>| {
        if let Err(e) = result {
            failures.push(format!("{what}: expected allow, got {e}"));
        }
    };
    allowed("read in share", read(&share.join("inside.txt")));
    allowed("read in share via /.vol", read(&vol_inside));
    allowed(
        "bind listed socket",
        UnixListener::bind(&socket_ok).map(drop),
    );

    let mut denied = |what: &str, result: std::io::Result<()>| match result {
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {}
        Err(e) => failures.push(format!("{what}: expected deny, got {e}")),
        Ok(()) => failures.push(format!("{what}: expected deny, got allow")),
    };
    denied("read outside", read(&outside));
    denied("read share/..", read(&share.join("../outside.txt")));
    denied("read abs link", read(&share.join("abs")));
    denied("read rel link", read(&share.join("rel")));
    denied("read outside via /.vol", read(&vol_outside));
    denied(
        "list share parent via /.vol",
        std::fs::read_dir(&vol_share_parent).map(drop),
    );
    denied(
        "write in ro share",
        std::fs::write(share.join("new.txt"), "x"),
    );
    denied(
        "tcp connect",
        std::net::TcpStream::connect(tcp_addr).map(drop),
    );
    denied(
        "bind other socket",
        UnixListener::bind(&socket_other).map(drop),
    );

    for failure in &failures {
        eprintln!("FAIL {failure}");
    }
    println!("all checks done");
    i32::from(!failures.is_empty())
}

/// Read the file at `path`.
fn read(path: &Path) -> std::io::Result<()> {
    std::fs::read(path).map(drop)
}
