//! `airlock` MCP server of the admin service: the protocol, the `execute`
//! errors and runs, and the rule tools.
//!
//! A network namespace needs root (a real VM). Thus the granted runs here
//! use the network of the test, and the bats tests cover the namespace.

use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::admin::{AdminState, Capabilities, RuleEditing};
use crate::test_cfg::{
    FakeDecision, FakeGrantHost, FakeRuleChange, FakeUpsert, GrantCall, block_on_local, call_tool,
    eventually_async, post_admin_mcp, serve_admin_mcp,
};

/// All agent features on.
const ALL: Capabilities = Capabilities {
    grants: true,
    rule_editing: RuleEditing::All,
};

/// Test that an MCP client can connect and find all tools, and that
/// requests from other origins and host names are refused.
///   1. Send `initialize` and check the server name and the tools capability
///   2. Send `tools/list` and check the tool names and the required
///      arguments
///   3. Send a request with a foreign `Origin` and check HTTP 403
///   4. Send a request with a foreign `Host` and check that it is refused
#[test]
fn admin_mcp_client_finds_execute_and_rule_tools() {
    block_on_local(async {
        let host = Rc::new(FakeGrantHost::default());
        let addr = serve_admin_mcp(AdminState::new(), &host, true, ALL).await;

        let init = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1.0" },
            },
        });
        let reply = post_admin_mcp(addr, &init, &[]).await;
        assert_eq!(reply.status, 200, "{reply:?}");
        let value: Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(value["result"]["serverInfo"]["name"], "airlock", "{value}");
        assert!(
            value["result"]["capabilities"]["tools"].is_object(),
            "{value}"
        );

        let list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let reply = post_admin_mcp(addr, &list, &[]).await;
        let value: Value = serde_json::from_str(&reply.body).unwrap();
        let mut tools: Vec<(String, Vec<String>)> = value["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let mut required: Vec<String> = t["inputSchema"]["required"]
                    .as_array()
                    .map(|r| r.iter().map(|v| v.as_str().unwrap().to_string()).collect())
                    .unwrap_or_default();
                required.sort_unstable();
                (t["name"].as_str().unwrap().to_string(), required)
            })
            .collect();
        tools.sort_unstable();
        let strings = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            tools,
            [
                ("execute".to_string(), strings(&["cmd"])),
                ("get_manual_entry".to_string(), strings(&["file"])),
                ("list_manual_entries".to_string(), vec![]),
                ("show_rules".to_string(), vec![]),
                ("upsert_rule".to_string(), strings(&["name"])),
            ],
            "{value}"
        );

        let reply = post_admin_mcp(addr, &list, &["Origin: http://evil.example"]).await;
        assert_eq!(reply.status, 403, "{reply:?}");
        let reply = post_admin_mcp(addr, &list, &["Origin: http://admin.airlock"]).await;
        assert_eq!(reply.status, 200, "{reply:?}");

        let reply = crate::test_cfg::http(
            addr,
            "POST",
            "/mcp",
            &[
                "Host: evil.example",
                "Content-Type: application/json",
                "Accept: application/json, text/event-stream",
            ],
            Some(&list),
        )
        .await;
        assert!(reply.status >= 400, "{reply:?}");
    });
}

/// Test that bad `execute` requests give a tool error and do not reach the
/// host. The user must not get approval requests that cannot run.
///   1. Call with both `hosts` and `rule`, with neither, with only blank
///      hosts, with a refused env key and with a malformed env entry
///   2. Call before the boot is complete
///   3. Check that each result is an error and that the host got no request
#[test]
fn execute_bad_request_gives_tool_error_without_host_request() {
    block_on_local(async {
        let host = Rc::new(FakeGrantHost::default());
        let addr = serve_admin_mcp(AdminState::new(), &host, true, ALL).await;

        for (args, needle) in [
            (
                json!({ "cmd": "true", "hosts": ["a.example"], "rule": "npm" }),
                "either",
            ),
            (json!({ "cmd": "true" }), "either"),
            (
                json!({ "cmd": "true", "hosts": [" "], "rule": "" }),
                "either",
            ),
            (
                json!({ "cmd": "true", "hosts": ["a.example"], "env": ["LD_PRELOAD=/tmp/x.so"] }),
                "LD_PRELOAD",
            ),
            (
                json!({ "cmd": "true", "rule": "npm", "env": ["https_proxy=http://x"] }),
                "https_proxy",
            ),
            (
                json!({ "cmd": "true", "rule": "npm", "env": ["NO_VALUE"] }),
                "NO_VALUE",
            ),
        ] {
            let result = call_tool(addr, "execute", args).await;
            assert_eq!(result["isError"], true, "{result}");
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(needle), "{text}");
        }

        let not_booted = serve_admin_mcp(AdminState::new(), &host, false, ALL).await;
        let result = call_tool(
            not_booted,
            "execute",
            json!({ "cmd": "true", "rule": "npm" }),
        )
        .await;
        assert_eq!(result["isError"], true, "{result}");

        assert!(host.calls.borrow().is_empty());
    });
}

/// Test that a deny by the user gives a normal result with
/// `approved: false`, and that another deny gives a tool error with the
/// reason. The agent must see the difference between a user decision and a
/// failure.
///   1. Call `execute` with a rule, an env entry and a host that denies as
///      the user
///   2. Check the result and the request that the host got. The request
///      has the default cwd from the boot
///   3. Call `execute` with a host that denies for another reason
///   4. Check that the result is an error with the reason
#[test]
fn execute_denied_by_user_gives_not_approved_result() {
    block_on_local(async {
        let host = Rc::new(FakeGrantHost::default());
        let addr = serve_admin_mcp(AdminState::new(), &host, true, ALL).await;
        let args = json!({ "cmd": "npm ci", "rule": " npm ", "env": ["MODE=ci"] });

        let result = call_tool(addr, "execute", args.clone()).await;
        assert_ne!(result["isError"], true, "{result}");
        assert_eq!(
            output_of(&result),
            json!({
                "approved": false, "exit_code": null, "stdout": "", "stderr": "", "hosts": [],
                "aborted": false,
            })
        );
        assert_eq!(
            *host.calls.borrow(),
            [GrantCall {
                cmd: "npm ci".into(),
                hosts: vec![],
                rule: "npm".into(),
                env: vec!["MODE=ci".into()],
                cwd: FakeGrantHost::BASE_CWD.into(),
            }]
        );

        let host = Rc::new(FakeGrantHost {
            decision: FakeDecision::DenyOther,
            ..FakeGrantHost::default()
        });
        let addr = serve_admin_mcp(AdminState::new(), &host, true, ALL).await;
        let result = call_tool(addr, "execute", args).await;
        assert_eq!(result["isError"], true, "{result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(FakeGrantHost::DENY_REASON), "{text}");
    });
}

/// Test that a granted request runs the command, returns the hosts of the
/// grant log, and releases the grant after the run. The agent must see the
/// blocked hosts, and the access must end with the command.
///   1. Grant a request for a command that writes to stdout and stderr
///   2. Check the exit code, the output and the hosts (IPv6 in brackets)
///   3. Check that the host sees the release of the grant proxy
#[test]
fn execute_granted_runs_command_and_returns_grant_log() {
    block_on_local(async {
        let host = Rc::new(FakeGrantHost {
            decision: FakeDecision::Grant,
            log: vec![
                ("registry.npmjs.org".into(), 443, false),
                ("2001:db8::1".into(), 80, true),
            ],
            ..FakeGrantHost::default()
        });
        let addr = serve_admin_mcp(AdminState::new(), &host, true, ALL).await;

        let result = call_tool(
            addr,
            "execute",
            json!({ "cmd": "echo hi; echo oops >&2; exit 3", "hosts": ["registry.npmjs.org"] }),
        )
        .await;
        assert_ne!(result["isError"], true, "{result}");
        assert_eq!(
            output_of(&result),
            json!({
                "approved": true,
                "exit_code": 3,
                "stdout": "hi\n",
                "stderr": "oops\n",
                "hosts": [
                    { "host": "registry.npmjs.org:443", "blocked": false },
                    { "host": "[2001:db8::1]:80", "blocked": true },
                ],
                "aborted": false,
            })
        );
        eventually_async("grant release", async || {
            (host.released.get() == 1).then_some(())
        })
        .await;
    });
}

/// Test that a user abort on the host stops the command and gives a result
/// with `aborted: true`. The user must be able to stop a long command.
///   1. Grant a request for a long command with a host that aborts soon
///   2. Check that the result comes before the command ends
///   3. Check `approved`, `aborted`, the output so far and no exit code
///   4. Check that the host sees the release of the grant proxy
#[test]
fn execute_aborted_by_user_stops_command() {
    block_on_local(async {
        let host = Rc::new(FakeGrantHost {
            decision: FakeDecision::Grant,
            abort_after: Some(Duration::from_millis(300)),
            ..FakeGrantHost::default()
        });
        let addr = serve_admin_mcp(AdminState::new(), &host, true, ALL).await;

        let start = Instant::now();
        let result = call_tool(
            addr,
            "execute",
            json!({ "cmd": "echo started; sleep 30", "hosts": ["a.example"] }),
        )
        .await;
        assert!(start.elapsed() < Duration::from_secs(10), "{result}");
        let output = output_of(&result);
        assert_eq!(output["approved"], true, "{output}");
        assert_eq!(output["aborted"], true, "{output}");
        assert_eq!(output["exit_code"], Value::Null, "{output}");
        assert_eq!(output["stdout"], "started\n", "{output}");
        eventually_async("grant release", async || {
            (host.released.get() == 1).then_some(())
        })
        .await;
    });
}

/// Test that `show_rules` returns the rules of the host as JSON. The agent
/// must find the rule names for `execute`.
///   1. Configure two rules in the fake host
///   2. Call `show_rules` and check the JSON array
#[test]
fn show_rules_returns_host_rules() {
    block_on_local(async {
        let strings = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        let host = Rc::new(FakeGrantHost {
            rules: vec![
                (
                    "npm".into(),
                    "npm registry".into(),
                    strings(&["registry.npmjs.org"]),
                    vec![],
                ),
                (
                    "gh".into(),
                    "GitHub".into(),
                    strings(&["*.github.com", "github.com:443"]),
                    strings(&["gist.github.com"]),
                ),
            ],
            ..FakeGrantHost::default()
        });
        let addr = serve_admin_mcp(AdminState::new(), &host, false, ALL).await;

        let result = call_tool(addr, "show_rules", json!({})).await;
        assert_ne!(result["isError"], true, "{result}");
        assert_eq!(
            output_of(&result),
            json!({ "rules": [
                { "name": "npm", "enabled": "on-demand", "description": "npm registry",
                  "allow": ["registry.npmjs.org"], "deny": [] },
                { "name": "gh", "enabled": "on-demand", "description": "GitHub",
                  "allow": ["*.github.com", "github.com:443"], "deny": ["gist.github.com"] },
            ]})
        );
    });
}

/// Test that `upsert_rule` sends only the given values to the host and maps
/// the host result: approve gives `approved: true`, a user deny gives
/// `approved: false`, and an error gives a tool error.
///   1. Call `upsert_rule` with an approving host and check the result and
///      the change that the host got
///   2. Call with only `enabled` and check that no list is sent
///   3. Call with a bad `enabled` value and check the tool error
///   4. Call with a host that denies as the user and check the result
///   5. Call with a host that refuses the rule and check the tool error
#[test]
fn upsert_rule_maps_host_decision() {
    block_on_local(async {
        let args = json!({
            "name": " npm ", "description": "npm registry",
            "allow": ["registry.npmjs.org", " "],
        });
        let call_with = async |upsert: FakeUpsert, args: Value| {
            let host = Rc::new(FakeGrantHost {
                upsert,
                ..FakeGrantHost::default()
            });
            let addr = serve_admin_mcp(AdminState::new(), &host, false, ALL).await;
            (call_tool(addr, "upsert_rule", args).await, host)
        };
        let call = async |upsert: FakeUpsert| call_with(upsert, args.clone()).await;

        let (result, host) = call(FakeUpsert::Approve).await;
        assert_eq!(output_of(&result), json!({ "approved": true }), "{result}");
        assert_eq!(
            *host.upserts.borrow(),
            [FakeRuleChange {
                name: "npm".into(),
                description: Some("npm registry".into()),
                allow: Some(vec!["registry.npmjs.org".into()]),
                ..FakeRuleChange::default()
            }]
        );

        let args = json!({ "name": "npm", "enabled": true });
        let (result, host) = call_with(FakeUpsert::Approve, args).await;
        assert_eq!(output_of(&result), json!({ "approved": true }), "{result}");
        assert_eq!(
            *host.upserts.borrow(),
            [FakeRuleChange {
                name: "npm".into(),
                enabled: "true".into(),
                ..FakeRuleChange::default()
            }]
        );
        let args = json!({ "name": "npm", "enabled": "yes" });
        let (result, host) = call_with(FakeUpsert::Approve, args).await;
        assert_eq!(result["isError"], true, "{result}");
        assert!(host.upserts.borrow().is_empty());

        let (result, _) = call(FakeUpsert::DenyByUser).await;
        assert_eq!(output_of(&result), json!({ "approved": false }), "{result}");

        let (result, _) = call(FakeUpsert::Error("bad pattern".into())).await;
        assert_eq!(result["isError"], true, "{result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("bad pattern"), "{text}");
    });
}

/// Test that the server lists only the tools and instructions of the
/// features that the host enabled. The agent must not see tools that can
/// only fail.
///   1. Serve the MCP server with each combination of grants and rule
///      editing
///   2. Check the tool names, the instructions and the output schemas
///   3. Call a tool that is not listed and check that it is refused
#[test]
fn tool_list_follows_capabilities() {
    block_on_local(async {
        let host = Rc::new(FakeGrantHost::default());
        let manual = ["get_manual_entry", "list_manual_entries"];
        let rules = ["show_rules", "upsert_rule"];
        for (grants, rule_editing) in [
            (false, RuleEditing::None),
            (true, RuleEditing::None),
            (false, RuleEditing::OnDemand),
            (true, RuleEditing::All),
        ] {
            let caps = Capabilities {
                grants,
                rule_editing,
            };
            let addr = serve_admin_mcp(AdminState::new(), &host, true, caps).await;
            let mut want: Vec<&str> = manual.to_vec();
            if grants {
                want.push("execute");
            }
            if rule_editing != RuleEditing::None {
                want.extend(rules);
            }
            want.sort_unstable();

            let list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
            let value: Value =
                serde_json::from_str(&post_admin_mcp(addr, &list, &[]).await.body).unwrap();
            let tools = value["result"]["tools"].as_array().unwrap();
            let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
            names.sort_unstable();
            assert_eq!(names, want, "{caps:?}");
            // All tools except the manual page text have typed output.
            for tool in tools {
                let typed = tool["name"] != "get_manual_entry";
                assert_eq!(tool["outputSchema"].is_object(), typed, "{tool}");
            }

            let init = json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" },
                },
            });
            let value: Value =
                serde_json::from_str(&post_admin_mcp(addr, &init, &[]).await.body).unwrap();
            let text = value["result"]["instructions"].as_str().unwrap();
            assert_eq!(text.contains("`execute`"), grants, "{text}");
            assert_eq!(
                text.contains("`upsert_rule`"),
                rule_editing != RuleEditing::None,
                "{text}"
            );
            assert!(text.contains("`get_manual_entry`"), "{text}");

            if !grants {
                let result =
                    call_tool(addr, "execute", json!({ "cmd": "true", "rule": "npm" })).await;
                assert_eq!(result, Value::Null, "{value}");
            }
        }
        assert!(host.calls.borrow().is_empty());
    });
}

/// Check that a tool result is not an error and return its JSON text. The
/// structured content must be the same JSON.
fn output_of(result: &Value) -> Value {
    assert_ne!(result["isError"], true, "{result}");
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["structuredContent"], text, "{result}");
    text
}
