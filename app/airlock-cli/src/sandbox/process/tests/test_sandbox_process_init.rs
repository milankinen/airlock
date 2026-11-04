//! Tests for the start of the sandbox process: the locks and the init data
//! go to the child, the ready message comes back, the parent connects and
//! then lets the child detach. Also the failures: an init that fails, and a
//! parent that stops before the detach. A bash script is the sandbox
//! process, so no VM is necessary. Bash, not sh: the descriptor numbers
//! can be over 9.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::cli::LogLevel;
use crate::sandbox::process::init::{LogOpts, Mode, ToChild};
use crate::sandbox::process::launch::{ExitedBeforeReady, LaunchRequest, Launched, spawn_program};
use crate::sandbox::lock::{self, HeldLock, LockKind};
use crate::test_cfg::{block_on_local, eventually, temp_dir};
use crate::util::VerifiedPath;

/// The verified form of the test path `path`.
fn verified(path: &Path) -> VerifiedPath {
    VerifiedPath::new_follow(path, None).unwrap()
}

/// Shell lines that read the init message from the socket `$1` into
/// `init.json`, and get the descriptors of the startup lock in `$startup`
/// and of the RPC socket in `$rpc`.
const READ_INIT: &str = r#"fd=$1
read -r init <&$fd
printf '%s\n' "$init" > init.json
startup=$(sed -n 's/.*"startup_lock_fd":\([0-9]*\).*/\1/p' init.json)
rpc=$(sed -n 's/.*"rpc_fd":\([0-9]*\).*/\1/p' init.json)
"#;

/// Take both locks of the sandbox `dir`.
fn lock_both(dir: &VerifiedPath) -> (HeldLock, HeldLock) {
    let run = lock::try_lock(dir, LockKind::Run).unwrap().unwrap();
    let startup = lock::try_lock(dir, LockKind::Startup).unwrap().unwrap();
    (run, startup)
}

/// Make the launch request for the sandbox `dir`.
fn request(dir: &VerifiedPath, capture_output: bool) -> LaunchRequest {
    LaunchRequest {
        sandbox_id: "abc".into(),
        cwd: dir.clone(),
        log: LogOpts {
            log_level: LogLevel::Debug,
            verbose: false,
            quiet: true,
        },
        mode: Mode::Exec,
        capture_output,
    }
}

/// Start `/bin/bash -c <script>` as the sandbox process of `dir`. The connect
/// step records if the child had no detach message at that time, and reads
/// one line from the RPC socket.
async fn spawn_script(
    script: &str,
    dir: &VerifiedPath,
    capture_output: bool,
) -> anyhow::Result<Launched<(bool, String)>> {
    let (run, startup) = lock_both(dir);
    let bash = verified(Path::new("/bin/bash"));
    let args: Vec<OsString> = vec!["-c".into(), script.into(), "bash".into()];
    let detach_file = dir.join("detach.json").unwrap();
    let connect = async |rpc: std::os::unix::net::UnixStream| {
        use std::io::BufRead;
        let before_detach = detach_file.symlink_metadata().is_err();
        let mut line = String::new();
        std::io::BufReader::new(rpc).read_line(&mut line)?;
        anyhow::Ok((before_detach, line))
    };
    spawn_program(
        &bash,
        &args,
        run,
        startup,
        &request(dir, capture_output),
        connect,
    )
    .await
}

/// Test that the child gets both locks, the RPC socket and the init data,
/// that the parent attaches after the ready message and before the
/// detach, and that the
/// init ends when the child closes its end. The run lock stays with the
/// child, and the detach frees the startup lock.
///   1. Start a child that saves the init data, writes a line to the RPC
///      socket, sends ready with a main command, saves the detach message,
///      closes the startup lock and the socket, and then sleeps
///   2. Check that the connect step ran before the detach message and got
///      the line of the RPC socket
///   3. Check the init data, the ready message and the detach message
///   4. Check that the child holds the run lock and the startup lock is
///      free
///   5. Stop the child and check that the run lock is free
#[test]
fn child_gets_locks_and_init_and_detach_ends_init() {
    let tmp = temp_dir();
    let dir = verified(tmp.path());
    let script = format!(
        r#"{READ_INIT}printf 'rpc\n' >&$rpc
printf '{{"ready":{{"main_argv":["echo","hi"]}}}}\n' >&$fd
read -r detach <&$fd
printf '%s\n' "$detach" > detach.json
printf '"detached"\n' >&$fd
eval "exec $startup>&- $fd>&-"
exec sleep 30 </dev/null >/dev/null 2>&1"#
    );

    let launched = block_on_local(spawn_script(&script, &dir, false)).unwrap();
    let (before_detach, rpc_line) = &launched.connected;
    assert!(before_detach, "connect ran after the detach message");
    assert_eq!(rpc_line, "rpc\n");
    let init = std::fs::read(tmp.path().join("init.json")).unwrap();
    let ToChild::Init(init) = serde_json::from_slice(&init).unwrap() else {
        panic!("first message is not the init data");
    };
    assert_eq!(init.sandbox_id, "abc");
    assert_eq!(init.mode, Mode::Exec);
    assert!(init.log.quiet);
    assert_eq!(
        launched.ready.main_argv,
        Some(vec!["echo".to_string(), "hi".to_string()])
    );
    let detach = std::fs::read(tmp.path().join("detach.json")).unwrap();
    assert_eq!(
        serde_json::from_slice::<ToChild>(&detach).unwrap(),
        ToChild::Detach
    );
    assert!(lock::is_held(&dir, LockKind::Run));
    eventually("startup lock is free", || {
        (!lock::is_held(&dir, LockKind::Startup)).then_some(())
    });

    let pid = i32::try_from(launched.child.pid).unwrap();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    eventually("run lock is free", || {
        (!lock::is_held(&dir, LockKind::Run)).then_some(())
    });
}

/// Test that a child that fails before it is ready gives its exit code,
/// and that a child with captured output gives the end of its output. The
/// parent then exits with the same code, and an agent client can show the
/// output.
///   1. Start a child that writes an error and exits with code 3
///   2. Check the exit code and that the connect step did not run
///   3. Start the same child with captured output
///   4. Check that the error has the output of the child
///   5. Check that both locks are free
#[test]
fn init_failure_gives_exit_code_and_captured_output() {
    let tmp = temp_dir();
    let dir = verified(tmp.path());
    let script = format!("{READ_INIT}echo 'no image' >&2; exit 3");

    let err = block_on_local(spawn_script(&script, &dir, false))
        .err()
        .unwrap();
    let exited = err.downcast_ref::<ExitedBeforeReady>().unwrap();
    assert_eq!(exited.code, 3);
    assert_eq!(exited.output, None);

    let err = block_on_local(spawn_script(&script, &dir, true))
        .err()
        .unwrap();
    let exited = err.downcast_ref::<ExitedBeforeReady>().unwrap();
    assert_eq!(exited.code, 3);
    assert_eq!(exited.output.as_deref(), Some("no image\n"));
    assert_eq!(exited.last_error.as_deref(), Some("no image"));
    for kind in [LockKind::Startup, LockKind::Run] {
        eventually("lock is free", || {
            (!lock::is_held(&dir, kind)).then_some(())
        });
    }
}

/// Test that the child sees the end of the stream when the parent stops
/// before the detach, and that the parent waits for the exit of the
/// child. A child must not run on after its parent failed.
///   1. Start a child that sends ready, and records a stream end before the
///      detach message
///   2. Let the connect step fail
///   3. Check the connect error, and that the child recorded the stream end
///      before the start returned
#[test]
fn parent_stop_before_detach_ends_child() {
    let tmp = temp_dir();
    let dir = verified(tmp.path());
    let script = format!(
        r#"{READ_INIT}printf '{{"ready":{{}}}}\n' >&$fd
if read -r detach <&$fd; then exit 0; fi
: > parent-gone
exit 5"#
    );
    let (run, startup) = lock_both(&dir);
    let bash = verified(Path::new("/bin/bash"));
    let args: Vec<OsString> = vec!["-c".into(), script.into(), "bash".into()];
    let connect = async |_rpc| -> anyhow::Result<()> { anyhow::bail!("connect failed") };

    let result = block_on_local(async {
        tokio::time::timeout(
            Duration::from_secs(10),
            spawn_program(&bash, &args, run, startup, &request(&dir, false), connect),
        )
        .await
        .expect("start did not end")
    });
    let err = result.err().unwrap();
    assert_eq!(err.to_string(), "connect failed");
    assert!(tmp.path().join("parent-gone").exists());
}

/// Test that a child that stops at the detach, with no detached message,
/// gives its exit code. A Ctrl+C at the end of the init must give its exit
/// code 130, not an RPC error of the next step.
///   1. Start a child that sends ready, reads the detach message and exits
///      with code 130
///   2. Check that the start gives the exit code 130
#[test]
fn child_stop_at_detach_gives_exit_code() {
    let tmp = temp_dir();
    let dir = verified(tmp.path());
    let script = format!(
        r#"{READ_INIT}printf 'rpc\n' >&$rpc
printf '{{"ready":{{}}}}\n' >&$fd
read -r detach <&$fd
exit 130"#
    );
    let err = block_on_local(spawn_script(&script, &dir, false))
        .err()
        .unwrap();
    let exited = err.downcast_ref::<ExitedBeforeReady>().unwrap();
    assert_eq!(exited.code, 130);
}

/// Test that a ready message with a long main command is accepted, and
/// that a ready message over the size limit ends the child. Long start
/// arguments must not stop the start, and a refused message must not let
/// both processes wait for each other with the locks held.
///   1. Start a child that sends ready with a main command over 64 KiB and
///      then detaches
///   2. Check that the start gives the full main command, and stop the
///      child
///   3. Start a child that sends a ready message over the size limit and
///      then waits for the detach message
///   4. Check that the start ends with the exit of the child, and that both
///      locks are free
#[test]
fn long_ready_message_is_accepted_and_too_large_one_ends_child() {
    let tmp = temp_dir();
    let dir = verified(tmp.path());
    let ready = |size: usize| {
        format!(
            r#"{READ_INIT}printf 'rpc\n' >&$rpc
long=$(head -c {size} /dev/zero | tr '\0' x)
printf '{{"ready":{{"main_argv":["%s"]}}}}\n' "$long" >&$fd
read -r detach <&$fd || exit 5
printf '"detached"\n' >&$fd
eval "exec $startup>&- $fd>&-"
exec sleep 30 </dev/null >/dev/null 2>&1"#
        )
    };
    let start = |script: String| {
        let dir = dir.clone();
        block_on_local(async move {
            tokio::time::timeout(Duration::from_secs(20), spawn_script(&script, &dir, false))
                .await
                .expect("start did not end")
        })
    };

    let launched = start(ready(200 * 1024)).unwrap();
    let argv = launched.ready.main_argv.clone().unwrap();
    assert_eq!(argv.len(), 1);
    assert_eq!(argv[0].len(), 200 * 1024);
    let pid = i32::try_from(launched.child.pid).unwrap();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    eventually("run lock is free", || {
        (!lock::is_held(&dir, LockKind::Run)).then_some(())
    });

    let err = start(ready(3 * 1024 * 1024)).err().unwrap();
    assert!(err.is::<ExitedBeforeReady>(), "{err:#}");
    for kind in [LockKind::Startup, LockKind::Run] {
        eventually("lock is free", || {
            (!lock::is_held(&dir, kind)).then_some(())
        });
    }
}
