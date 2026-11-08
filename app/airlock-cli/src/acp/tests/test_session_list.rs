//! Tests for the session list and its session dirs.

use serde_json::{Value, json};

use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Session rows of the fake agent, with guest session dirs.
fn rows() -> Value {
    json!({ "sessions": [
        { "sessionId": "a", "cwd": "/work" },
        { "sessionId": "b", "cwd": "/work/sub" },
        { "sessionId": "c", "cwd": "/work2" },
        { "sessionId": "d", "cwd": "/work/../etc" },
        { "sessionId": "e", "cwd": "/other" },
    ]})
}

/// Get the session dirs of a `session/list` response.
fn dirs(response: &Value) -> Vec<&str> {
    response["result"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("{response}"))
        .iter()
        .map(|row| row["cwd"].as_str().unwrap())
        .collect()
}

/// Test that the client gets an empty session list before the agent runs.
/// The list without a project dir cannot start the agent, and an error
/// makes the editor show a failure.
///   1. Do the client handshake
///   2. Send `session/list` without a dir
///   3. Check the empty list and that no agent started
#[test]
fn session_list_before_agent_start_is_empty() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[]);
        let launches = launcher.launches.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let (response, _) = client.request(1, "session/list", json!({})).await;
        assert_eq!(response["result"], json!({ "sessions": [] }));
        assert_eq!(launches.get(), 0);
    });
}

/// Test that a session list with a session dir runs in a one-shot agent of
/// that sandbox, and that its session dirs change from guest paths to host
/// paths by the project dirs, in both modes. The editor lists sessions
/// before it opens one, and shows only the sessions of its dir.
///   1. Use an agent that lists sessions in the guest dir, in a sub dir,
///      in a dir with the same prefix, with `..` and in another dir
///   2. Send `session/list` in the project dir with no open session
///   3. Check that only the rows in the guest project dir change, and that
///      one agent launched
///   4. Send a list that gets an error and check the error
///   5. Check that the second list launched a new agent: each list runs
///      in its own agent
#[test]
fn session_list_runs_in_one_shot_agent_with_host_paths() {
    let list = rows().to_string();
    for restrict in [true, false] {
        block_on_local(async {
            let launcher = LocalProcessLauncher::new(&[("AGENT_LIST", &list)]);
            let launches = launcher.launches.clone();
            let mut client = TestClient::start(launcher, test_config(restrict));
            client.initialize(json!({})).await;

            let params = json!({ "cwd": "/projects/a" });
            let (response, _) = client.request(1, "session/list", params).await;
            assert_eq!(
                dirs(&response),
                [
                    "/projects/a",
                    "/projects/a/sub",
                    "/work2",
                    "/work/../etc",
                    "/other"
                ]
            );
            assert_eq!(launches.get(), 1);

            let params = json!({ "cwd": "/projects/a/sub", "cursor": "bad" });
            let (failed, _) = client.request(2, "session/list", params).await;
            assert!(failed.get("error").is_some(), "{failed}");
            assert_eq!(launches.get(), 2);
        });
    }
}

/// Test that a session list in a sandbox with an open session runs in its
/// own agent, and that the session keeps working. A list must not end the
/// work of the user.
///   1. Start a session in sandbox `a`
///   2. Send `session/list` in the sandbox and check the host path row
///   3. Check that a second agent launched for the list
///   4. Send a prompt to the session and check its response
#[test]
fn session_list_beside_open_session_runs_in_own_agent() {
    block_on_local(async {
        let list = json!({ "sessions": [{ "sessionId": "x", "cwd": "/work" }] });
        let launcher = LocalProcessLauncher::new(&[("AGENT_LIST", &list.to_string())]);
        let launches = launcher.launches.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let sid = client.new_session(1, "/projects/a").await;

        let params = json!({ "cwd": "/projects/a" });
        let (response, _) = client.request(2, "session/list", params).await;
        assert_eq!(dirs(&response), ["/projects/a"]);
        assert_eq!(launches.get(), 2);
        let (response, _) = client.prompt(3, &sid).await;
        assert!(response.get("result").is_some(), "{response}");
    });
}

/// Test that the stop of a one-shot list agent does not set the proxy
/// exit code. The agent stops because the list completed, not because it
/// failed.
///   1. Use an agent that exits with code 5 at the end of its input
///   2. Send `session/list` in sandbox `a` and check the response
///   3. Close the client and check the exit code 0
#[test]
fn one_shot_list_agent_stop_gives_exit_code_0() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[("AGENT_EOF", "5")]);
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": "/projects/a" });
        let (response, _) = client.request(1, "session/list", params).await;
        assert!(response.get("result").is_some(), "{response}");
        assert_eq!(client.exit_code().await, 0);
    });
}

/// Test that a session list in a dir with no sandbox gets an empty list,
/// and that other requests there still get an error. Editors list the
/// sessions of old dirs, for example removed git worktrees. An error makes
/// the editor show a failure.
///   1. Send `session/list` in a dir with no sandbox
///   2. Check the empty list and that no agent started
///   3. Send `session/new` in the same dir and check the error
#[test]
fn session_list_in_dir_without_sandbox_is_empty() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[]);
        let launches = launcher.launches.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;

        let params = json!({ "cwd": "/gone/worktree" });
        let (response, _) = client.request(1, "session/list", params).await;
        assert_eq!(response["result"], json!({ "sessions": [] }), "{response}");
        assert_eq!(launches.get(), 0);

        let params = json!({ "cwd": "/gone/worktree", "mcpServers": [] });
        let (failed, _) = client.request(2, "session/new", params).await;
        assert!(failed.get("error").is_some(), "{failed}");
    });
}
