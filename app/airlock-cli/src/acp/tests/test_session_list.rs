//! Tests for the session list and its session dirs.

use serde_json::{Value, json};

use crate::test_cfg::acp::{LocalStarter, TestClient, test_config};
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
        let starter = LocalStarter::new(&[]);
        let starts = starter.starts.clone();
        let mut client = TestClient::start(starter, test_config(true));
        client.initialize(json!({})).await;
        let (response, _) = client.request(1, "session/list", json!({})).await;
        assert_eq!(response["result"], json!({ "sessions": [] }));
        assert_eq!(starts.get(), 0);
    });
}

/// Test that the session dirs in a session list change from guest paths
/// to host paths, in both modes. The guest project dir is not the host
/// dir, and the editor shows only the sessions of its dir.
///   1. Use an agent that lists sessions in the guest dir, in a sub dir,
///      in a dir with the same prefix, with `..` and in another dir
///   2. Send `session/list` in the project dir and check that only the
///      rows in the guest dir change
///   3. Send a second page in the sub dir and check that the page uses
///      its own dir
///   4. Send a list that gets an error, and then a list with the same id
///      and no dir. Check that the rows of the second list do not change
#[test]
fn session_list_dirs_map_to_host_paths() {
    let list = rows().to_string();
    for restrict in [true, false] {
        block_on_local(async {
            let starter = LocalStarter::new(&[("AGENT_LIST", &list)]);
            let mut client =
                TestClient::session(starter, test_config(restrict), "/projects/a").await;

            let params = json!({ "cwd": "/projects/a" });
            let (response, _) = client.request(3, "session/list", params).await;
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

            let params = json!({ "cwd": "/projects/a/sub", "cursor": "c2" });
            let (response, _) = client.request(4, "session/list", params).await;
            assert_eq!(
                dirs(&response),
                [
                    "/work",
                    "/projects/a/sub",
                    "/work2",
                    "/work/../etc",
                    "/other"
                ]
            );

            let params = json!({ "cwd": "/projects/a", "cursor": "bad" });
            let (response, _) = client.request(5, "session/list", params).await;
            assert!(response.get("error").is_some(), "{response}");
            let (response, _) = client.request(5, "session/list", json!({})).await;
            assert_eq!(response["result"], rows());
        });
    }
}
