//! Tests for a process that runs attached to the terminal over the CLI
//! socket: the command, cwd, env, output and exit code go through, and a
//! sandbox stop ends the process with exit code 1. The tests use a real
//! CLI server on a real Unix socket. Only the guest and the terminal are
//! fake.

use std::cell::RefCell;
use std::rc::Rc;

use airlock_common::supervisor_capnp::stdin;

use crate::runtime::{MonitorSource, OutputSink, PtySize, Runtime, SignalStream, Terminal};
use crate::sandbox::process::attach::{AttachRequest, run_attached};
use crate::sandbox::process::vm::io::closed_stdin;
use crate::test_cfg::cli_server::{CliClient, CliSandbox, ExecGuest, SANDBOX_CWD, Spawned};
use crate::test_cfg::sinks::RecordingSink;
use crate::test_cfg::{block_on_local, eventually_async};

/// Runtime with no input and no signals. The terminal records the output.
#[derive(Default)]
struct RecordingRuntime(Rc<RefCell<RecordingSink>>);

/// Terminal of [`RecordingRuntime`].
struct RecordingTerminal(Rc<RefCell<RecordingSink>>);

impl Runtime for RecordingRuntime {
    type Terminal = RecordingTerminal;

    fn attach_stdin(&mut self) -> anyhow::Result<(stdin::Client, PtySize)> {
        Ok((closed_stdin(), None))
    }

    fn signals(&mut self) -> anyhow::Result<SignalStream> {
        Ok(Box::pin(futures::stream::pending()))
    }

    fn launch(self, _monitor: Option<MonitorSource>) -> anyhow::Result<RecordingTerminal> {
        Ok(RecordingTerminal(self.0))
    }
}

impl OutputSink for RecordingTerminal {
    fn stdout(&mut self, bytes: &[u8]) {
        self.0.borrow_mut().stdout(bytes);
    }

    fn stderr(&mut self, bytes: &[u8]) {
        self.0.borrow_mut().stderr(bytes);
    }
}

impl Terminal for RecordingTerminal {
    fn exit(self, exit_code: i32) -> i32 {
        exit_code
    }
}

/// Test that the attached process gets its command, cwd and env, and
/// that its output and exit code come back. An empty cwd is the sandbox
/// cwd.
///   1. Serve a sandbox whose process writes output and exits with 7
///   2. Run a command with a cwd and an env value
///   3. Check the exit code, the output and the spawn of the guest
///   4. Run the command again with an empty cwd and check the cwd
#[test]
fn attached_process_gets_cwd_and_env_and_gives_exit_code() {
    block_on_local(async {
        let (guest, spawned) = ExecGuest::client(b"hello", Some(7));
        let sandbox = CliSandbox::start("", false, guest).await;
        let client = CliClient::connect(&sandbox.sock).await;
        let argv = ["tool".to_string(), "-x".to_string()];
        let env = ["KEY=value".to_string()];
        let run = async |cwd: &str| {
            let runtime = RecordingRuntime::default();
            let output = runtime.0.clone();
            let request = AttachRequest {
                argv: &argv,
                cwd,
                env: &env,
                dump_dir: None,
            };
            let code = run_attached(&client.service, runtime, None, &request)
                .await
                .unwrap();
            let out = output.borrow().out.clone();
            (code, out)
        };

        assert_eq!(run("/work/sub").await, (7, b"hello".to_vec()));
        assert_eq!(
            spawned.borrow()[0],
            Spawned {
                cmd: "tool".into(),
                args: vec!["-x".into()],
                cwd: "/work/sub".into(),
                env: vec!["KEY=value".into()],
            }
        );
        run("").await;
        assert_eq!(spawned.borrow()[1].cwd, SANDBOX_CWD);
    });
}

/// Test that a sandbox stop while the process runs ends the attached run
/// with exit code 1, and not with a hang.
///   1. Serve a sandbox whose process never exits
///   2. Start the attached run and wait for the spawn
///   3. Stop the server, as the sandbox shutdown does
///   4. Check that the run ends with exit code 1
#[test]
fn sandbox_stop_during_attached_process_gives_exit_1() {
    block_on_local(async {
        let (guest, spawned) = ExecGuest::client(b"", None);
        let sandbox = CliSandbox::start("", false, guest).await;
        let client = CliClient::connect(&sandbox.sock).await;
        let service = client.service.clone();
        let run = tokio::task::spawn_local(async move {
            let argv = ["sleep".to_string()];
            let request = AttachRequest {
                argv: &argv,
                cwd: "",
                env: &[],
                dump_dir: None,
            };
            run_attached(&service, RecordingRuntime::default(), None, &request).await
        });
        eventually_async("process spawn", async || {
            (!spawned.borrow().is_empty()).then_some(())
        })
        .await;

        sandbox.server.abort();
        let code = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("attached run did not end")
            .unwrap()
            .unwrap();
        assert_eq!(code, 1);
    });
}
