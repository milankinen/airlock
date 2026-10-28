//! Tests for MCP sessions with real host server processes, served to the
//! guest over RPC: message relay, env, grants, process stop, idle timeout
//! and session limit.

use std::path::Path;
use std::time::Duration;

use airlock_common::supervisor_capnp::mcp_host;

use crate::test_cfg::mcp_session::{Event, next, open, send};
use crate::test_cfg::oci::verified_dir;
use crate::test_cfg::{
    block_on_local, host_env_vault, resolve_project_toml, rpc_loopback, temp_dir,
};

/// Serve the `[mcp]` servers of the project file `toml` to a guest client
/// over RPC. The host env has `HOST_VAR=from-host`, and the servers run in
/// `cwd`.
fn served(toml: &str, cwd: &Path) -> mcp_host::Client {
    let config = resolve_project_toml(toml).unwrap().values;
    let vault = host_env_vault(&[("HOST_VAR", "from-host")]);
    let cwd = verified_dir(cwd);
    let host = crate::mcp::resolve(&config.mcp, &vault, &cwd)
        .unwrap()
        .expect("enabled servers");
    rpc_loopback(capnp_rpc::new_client::<mcp_host::Client, _>(host).client)
}

/// Check if the process `pid` runs. A zombie does not run.
fn runs(pid: i32) -> bool {
    // SAFETY: signal 0 only checks the process.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    // Linux: a killed process can stay a zombie until its new parent
    // reaps it.
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
        !stat.rsplit(')').next().unwrap_or("").starts_with(" Z")
    })
}

/// Echo server: it writes its env value and its directory, then each
/// input line back. On the line `quit`, it exits with code 3.
const ECHO: &str = r#"
[mcp.echo]
command = "sh"
args = ["-c", '''
echo "start $GREETING $(pwd -P)"
while read -r l; do
  [ "$l" = quit ] && exit 3
  echo "$l"
done
''']
env.GREETING = "${HOST_VAR}"

[mcp.off]
enabled = false
command = "sh"
"#;

/// Test that a session relays the messages in both directions in order,
/// with the resolved env and the project directory, and tells the guest
/// when the server exits.
///   1. Open a session of an echo server
///   2. Check the first line: the env value from the host and the directory
///   3. Send three messages and check that they come back in order
///   4. Send a message with a line break and check that it is refused
///   5. Make the server exit and check the close reason
#[test]
fn session_relays_messages_in_order_and_reports_exit() {
    let dir = temp_dir();
    let cwd = dir.path().canonicalize().unwrap();
    block_on_local(async {
        let host = served(ECHO, &cwd);
        let (session, mut events) = open(&host, "echo", false).await.unwrap();

        assert_eq!(
            next(&mut events).await,
            Event::Message(format!("start from-host {}", cwd.display()))
        );
        for message in [r#"{"id":1}"#, r#"{"id":2}"#, r#"{"id":3}"#] {
            send(&session, message).await.unwrap();
        }
        for message in [r#"{"id":1}"#, r#"{"id":2}"#, r#"{"id":3}"#] {
            assert_eq!(next(&mut events).await, Event::Message(message.into()));
        }

        let err = send(&session, "{}\nquit").await.unwrap_err();
        assert!(err.contains("must be one line"), "{err}");

        send(&session, "quit").await.unwrap();
        let Event::Closed(reason) = next(&mut events).await else {
            panic!("no close");
        };
        assert!(reason.contains("exit status: 3"), "{reason}");
    });
}

/// Test that the guest can open only the servers that the config enables.
///   1. Open a server that is not in the config
///   2. Open a server that the config disables
///   3. Check that both are refused
#[test]
fn unknown_or_disabled_server_is_refused() {
    let dir = temp_dir();
    block_on_local(async {
        let host = served(ECHO, dir.path());
        for name in ["nope", "off"] {
            let Err(err) = open(&host, name, false).await else {
                panic!("{name} opened");
            };
            assert!(
                err.contains(&format!("unknown MCP server `{name}`")),
                "{err}"
            );
        }
    });
}

/// Test that the release of a session stops the server and the processes
/// that it started. Otherwise tools that a server starts stay on the host.
///   1. Open a server that starts a background process and writes both
///      process IDs
///   2. Release the session
///   3. Check that the guest gets the close and that both processes stop
#[test]
fn released_session_stops_server_and_its_children() {
    let dir = temp_dir();
    block_on_local(async {
        let host = served(
            r#"
            [mcp.tree]
            command = "sh"
            args = ["-c", "sleep 1000 & echo $$ $!; wait"]
            "#,
            dir.path(),
        );
        let (session, mut events) = open(&host, "tree", false).await.unwrap();
        let Event::Message(pids) = next(&mut events).await else {
            panic!("no process IDs");
        };
        let pids: Vec<i32> = pids.split(' ').map(|p| p.parse().unwrap()).collect();
        assert!(pids.iter().all(|&pid| runs(pid)), "{pids:?}");

        drop(session);
        // The background process has the same stdout. Thus the close comes
        // only after both processes stopped.
        assert!(matches!(next(&mut events).await, Event::Closed(_)));
        wait_stopped(&pids).await;
    });
}

/// Wait until none of the processes `pids` runs. Panics after 5 seconds.
async fn wait_stopped(pids: &[i32]) {
    for _ in 0..100 {
        if !pids.iter().any(|&pid| runs(pid)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("processes still run: {pids:?}");
}

/// Test that a server that closes its stdout but continues to run is
/// stopped. It can send nothing more, so the guest must learn that the
/// session ended.
///   1. Open a server that writes its process ID, closes its stdout and
///      waits
///   2. Check that the guest gets the close and that the process stops
#[test]
fn server_that_closes_stdout_is_stopped() {
    let dir = temp_dir();
    block_on_local(async {
        let host = served(
            r#"
            [mcp.mute]
            command = "sh"
            args = ["-c", "echo $$; exec >&-; sleep 1000"]
            "#,
            dir.path(),
        );
        let (_session, mut events) = open(&host, "mute", false).await.unwrap();
        let Event::Message(pid) = next(&mut events).await else {
            panic!("no process ID");
        };
        let Event::Closed(reason) = next(&mut events).await else {
            panic!("no close");
        };
        assert!(reason.contains("SIGKILL"), "{reason}");
        wait_stopped(&[pid.parse().unwrap()]).await;
    });
}

/// Test that the server stops when the guest can no longer get its
/// messages. A session that cannot send output has no use.
///   1. Open a server with a sink that fails each message, and keep the
///      session
///   2. Check that the sink got the process ID and that the process stops
#[test]
fn failing_guest_sink_stops_server() {
    let dir = temp_dir();
    block_on_local(async {
        let host = served(
            r#"
            [mcp.chatty]
            command = "sh"
            args = ["-c", "echo $$; sleep 1000"]
            "#,
            dir.path(),
        );
        let (_session, mut events) = open(&host, "chatty", true).await.unwrap();
        let Event::Message(pid) = next(&mut events).await else {
            panic!("no process ID");
        };
        wait_stopped(&[pid.parse().unwrap()]).await;
    });
}

/// Test that `send` does not wait for a server that does not read its
/// stdin. Otherwise one bad server blocks the guest.
///   1. Open a server that never reads its stdin
///   2. Send 64 KiB messages until a send fails
///   3. Check the error
#[test]
fn send_to_server_that_does_not_read_fails_without_blocking() {
    let dir = temp_dir();
    block_on_local(async {
        let host = served(
            "[mcp.deaf]\ncommand = \"sleep\"\nargs = [\"1000\"]\n",
            dir.path(),
        );
        let (session, _events) = open(&host, "deaf", false).await.unwrap();
        let message = "x".repeat(64 * 1024);
        for _ in 0..200 {
            let sent = tokio::time::timeout(Duration::from_secs(10), send(&session, &message))
                .await
                .expect("send in time");
            if let Err(err) = sent {
                assert!(err.contains("does not read its input"), "{err}");
                return;
            }
        }
        panic!("all sends succeeded");
    });
}

/// Test that a server line over the size limit stops the session. A bad
/// server must not make the host keep data without limit.
///   1. Open a server that writes one line longer than 16 MiB, then waits
///   2. Check that the guest gets no message and a close with the reason
#[test]
fn too_long_server_line_stops_session() {
    let dir = temp_dir();
    block_on_local(async {
        let host = served(
            r#"
            [mcp.big]
            command = "sh"
            args = ["-c", "head -c 17000000 /dev/zero | tr '\\0' a; echo; sleep 1000"]
            "#,
            dir.path(),
        );
        let (_session, mut events) = open(&host, "big", false).await.unwrap();
        let Event::Closed(reason) = next(&mut events).await else {
            panic!("message over the limit");
        };
        assert!(reason.contains("longer than 16777216 bytes"), "{reason}");
    });
}

/// Test that a server env template that reads an undefined host variable
/// stops the sandbox start, and that the error names the entry.
///   1. Resolve a server with an env template of an undefined variable
///   2. Check that the error names the server and the key
#[test]
fn undefined_env_variable_fails_resolve() {
    let config = resolve_project_toml("[mcp.x]\ncommand = \"sh\"\nenv.KEY = \"${NOPE}\"\n")
        .unwrap()
        .values;
    let vault = host_env_vault(&[]);
    let Err(err) = crate::mcp::resolve(&config.mcp, &vault, &verified_dir(Path::new("/"))) else {
        panic!("resolved");
    };
    assert!(format!("{err:#}").contains("mcp.x.env.KEY"), "{err:#}");
}

/// Test that the host stops a session without traffic after the idle
/// timeout. Else an MCP server that the guest forgot runs until the VM
/// stops.
///   1. Open a session of an echo server with a 1 s timeout
///   2. Check that the guest gets the close with the idle reason, after
///      1 s and before 3 s
#[test]
fn idle_session_is_stopped_after_timeout() {
    let dir = temp_dir();
    block_on_local(async {
        let host = served("[mcp.idle]\ncommand = \"cat\"\ntimeout = 1\n", dir.path());
        let started = std::time::Instant::now();
        let (_session, mut events) = open(&host, "idle", false).await.unwrap();
        assert_eq!(
            next(&mut events).await,
            Event::Closed("MCP session idle timeout (1 s)".into())
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(3),
            "{elapsed:?}"
        );
    });
}

/// Test that steady traffic keeps a session. Only a session without
/// traffic for the full timeout is idle.
///   1. Open a session of an echo server with a 1 s timeout
///   2. Send a message every 300 ms for 2 s and check each echo
///   3. Check that no close came
#[test]
fn session_with_steady_traffic_is_not_stopped() {
    let dir = temp_dir();
    block_on_local(async {
        let host = served("[mcp.busy]\ncommand = \"cat\"\ntimeout = 1\n", dir.path());
        let (session, mut events) = open(&host, "busy", false).await.unwrap();
        for i in 0..7 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            send(&session, &i.to_string()).await.unwrap();
            assert_eq!(next(&mut events).await, Event::Message(i.to_string()));
        }
        assert!(events.try_recv().is_err());
    });
}

/// Test that `timeout = 0` never stops an idle session.
///   1. Open a session of a server with `timeout = 0`
///   2. Wait 1.5 s without traffic and check that no close came
///   3. Send a message and check the echo
#[test]
fn session_with_zero_timeout_is_not_stopped() {
    let dir = temp_dir();
    block_on_local(async {
        let host = served(
            "[mcp.forever]\ncommand = \"cat\"\ntimeout = 0\n",
            dir.path(),
        );
        let (session, mut events) = open(&host, "forever", false).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(events.try_recv().is_err());
        send(&session, "x").await.unwrap();
        assert_eq!(next(&mut events).await, Event::Message("x".into()));
    });
}

/// Test that a new session over the session limit stops the session with
/// the oldest activity, and keeps the others. Thus a guest cannot start
/// servers without limit.
///   1. Open sessions A and B of an echo server with a limit of 2
///   2. Send a message on A, so that B has the oldest activity
///   3. Open session C and check that B gets the close with the limit
///      reason
///   4. Check that A still works
#[test]
fn session_over_limit_stops_least_recently_active_session() {
    let dir = temp_dir();
    let toml = ECHO.replacen("[mcp.echo]\n", "[mcp.echo]\nmax_sessions = 2\n", 1);
    block_on_local(async {
        let host = served(&toml, dir.path());
        let (a, mut a_events) = open(&host, "echo", false).await.unwrap();
        let (_b, mut b_events) = open(&host, "echo", false).await.unwrap();
        for events in [&mut a_events, &mut b_events] {
            assert!(matches!(next(events).await, Event::Message(_)));
        }
        send(&a, "a").await.unwrap();
        assert_eq!(next(&mut a_events).await, Event::Message("a".into()));

        let (_c, mut c_events) = open(&host, "echo", false).await.unwrap();
        assert!(matches!(next(&mut c_events).await, Event::Message(_)));
        assert_eq!(
            next(&mut b_events).await,
            Event::Closed("MCP session limit reached (2 sessions)".into())
        );
        send(&a, "still").await.unwrap();
        assert_eq!(next(&mut a_events).await, Event::Message("still".into()));
    });
}

/// Test that a session whose server exits frees its place in the session
/// limit. Else stopped servers would cause the stop of working sessions.
///   1. Open a session of an echo server with a limit of 1
///   2. Make the server exit and wait for the close
///   3. Open two more sessions one after the other. Check that the first
///      gets the limit stop only when the second opens, and that the
///      second works
#[test]
fn exited_session_frees_its_place_in_limit() {
    let dir = temp_dir();
    let toml = ECHO.replacen("[mcp.echo]\n", "[mcp.echo]\nmax_sessions = 1\n", 1);
    block_on_local(async {
        let host = served(&toml, dir.path());
        let (first, mut first_events) = open(&host, "echo", false).await.unwrap();
        assert!(matches!(next(&mut first_events).await, Event::Message(_)));
        send(&first, "quit").await.unwrap();
        let Event::Closed(reason) = next(&mut first_events).await else {
            panic!("no close");
        };
        assert!(reason.contains("exit status: 3"), "{reason}");

        let (second, mut second_events) = open(&host, "echo", false).await.unwrap();
        assert!(matches!(next(&mut second_events).await, Event::Message(_)));
        send(&second, "ok").await.unwrap();
        assert_eq!(next(&mut second_events).await, Event::Message("ok".into()));

        let (_third, mut third_events) = open(&host, "echo", false).await.unwrap();
        assert!(matches!(next(&mut third_events).await, Event::Message(_)));
        assert_eq!(
            next(&mut second_events).await,
            Event::Closed("MCP session limit reached (1 sessions)".into())
        );
    });
}

/// Test that the close of the host stops its live sessions and refuses
/// new ones. The owner closes the host this way after the host moved into
/// an RPC capability.
///   1. Open a session of a server that writes its process ID
///   2. Close the host with its stopper
///   3. Check that the guest gets the close with the given reason and
///      that the process stops
///   4. Check that a new open fails
#[test]
fn closed_host_stops_sessions_and_refuses_open() {
    let dir = temp_dir();
    block_on_local(async {
        let config = resolve_project_toml(
            "[mcp.pid]\ncommand = \"sh\"\nargs = [\"-c\", \"echo $$; exec cat\"]\n",
        )
        .unwrap()
        .values;
        let host =
            crate::mcp::resolve(&config.mcp, &host_env_vault(&[]), &verified_dir(dir.path()))
                .unwrap()
                .expect("enabled servers");
        let stopper = host.stopper();
        let host = rpc_loopback(capnp_rpc::new_client::<mcp_host::Client, _>(host).client);
        let (_session, mut events) = open(&host, "pid", false).await.unwrap();
        let Event::Message(pid) = next(&mut events).await else {
            panic!("no process ID");
        };

        stopper.close("editor session ended");
        assert_eq!(
            next(&mut events).await,
            Event::Closed("editor session ended".into())
        );
        wait_stopped(&[pid.parse().unwrap()]).await;

        let Err(err) = open(&host, "pid", false).await else {
            panic!("closed host opened a session");
        };
        assert!(err.contains("MCP server `pid` is closed"), "{err}");
    });
}
