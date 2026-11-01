//! Helpers for tests of the admin service.

use std::cell::{Cell, RefCell};
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use airlock_common::network_capnp::network_proxy;
use airlock_test_utils::{read_until_eof, rpc_loopback, serve};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::mcp::{HttpReply, http};
use crate::admin::mcp::GrantBridge;
use crate::admin::server::router;
use crate::admin::{AdminState, Capabilities, GrantContext, SpawnCreds};
use crate::net::GrantRoutes;

/// Serve the admin routes for `state` on a local port. Temporary network
/// access is not available.
pub(crate) async fn serve_admin(state: Arc<AdminState>) -> SocketAddr {
    serve(router(state, None, Capabilities::default())).await
}

/// Send a JSON hook payload with POST, as the HTTP hooks of Claude Code do.
/// Check that the status is 200 and return the JSON response body.
pub(crate) async fn post_hook(addr: SocketAddr, path: &str, body: &Value) -> Value {
    let body = body.to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: admin.airlock\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let response = read_until_eof(&mut stream).await;
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{response}");
    serde_json::from_str(body).unwrap()
}

/// One `requestGrant` call that the fake host got.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct GrantCall {
    /// Command text.
    pub cmd: String,
    /// Requested hosts.
    pub hosts: Vec<String>,
    /// Requested rule name.
    pub rule: String,
    /// More environment of the command.
    pub env: Vec<String>,
    /// Working directory of the command. Empty for the default.
    pub cwd: String,
}

/// Decision of [`FakeGrantHost`] for each grant request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum FakeDecision {
    /// Grant the request.
    Grant,
    /// Deny the request as the user.
    #[default]
    DenyByUser,
    /// Deny the request for another reason, with [`FakeGrantHost::DENY_REASON`].
    DenyOther,
}

/// Result of [`FakeGrantHost`] for each `upsertRule` call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum FakeUpsert {
    /// Approve the rule.
    #[default]
    Approve,
    /// Deny the rule as the user.
    DenyByUser,
    /// Refuse the rule with an error text.
    Error(String),
}

/// One on-demand rule in the fake host: `(name, description, allow,
/// deny)`.
pub(crate) type FakeRule = (String, String, Vec<String>, Vec<String>);

/// One `upsertRule` call that the fake host got.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct FakeRuleChange {
    /// Name of the rule.
    pub name: String,
    /// `enabled` text. Empty if not given.
    pub enabled: String,
    /// New description, if given.
    pub description: Option<String>,
    /// New allow patterns, if given.
    pub allow: Option<Vec<String>>,
    /// New deny patterns, if given.
    pub deny: Option<Vec<String>>,
}

/// A fake host `NetworkProxy` that records the grant and rule requests. By
/// default it denies each grant as the user and approves each rule. Call
/// [`FakeGrantHost::client`] in a `LocalSet`.
#[derive(Default)]
pub(crate) struct FakeGrantHost {
    /// Grant requests in order.
    pub calls: RefCell<Vec<GrantCall>>,
    /// Decision for each grant request.
    pub decision: FakeDecision,
    /// Entries that the grant proxy returns from `grantLog`:
    /// `(host, port, blocked)`.
    pub log: Vec<(String, u16, bool)>,
    /// Rules that `listRules` returns.
    pub rules: Vec<FakeRule>,
    /// Rules of the `upsertRule` calls, in order.
    pub upserts: RefCell<Vec<FakeRuleChange>>,
    /// Result of each `upsertRule` call.
    pub upsert: FakeUpsert,
    /// Number of grant proxies that the guest released.
    pub released: Cell<usize>,
    /// Time after which the grant proxy answers `waitRevoked` as a user
    /// abort. `None` keeps the call open.
    pub abort_after: Option<Duration>,
}

impl FakeGrantHost {
    /// Reason text of each deny that is not by the user.
    pub const DENY_REASON: &str = "the approval timed out";

    /// Base environment of the commands, as the boot gives it.
    pub const BASE_ENV: &str = "PATH=/usr/bin:/bin";

    /// Default working directory of the commands, as the boot gives it.
    pub const BASE_CWD: &str = "/";

    /// Duration of each grant in seconds.
    pub const GRANT_SECS: u32 = 30;

    /// Serve the fake over a real RPC connection and return the client.
    pub fn client(self: &Rc<Self>) -> network_proxy::Client {
        rpc_loopback(
            capnp_rpc::new_client::<network_proxy::Client, _>(FakeServer(self.clone())).client,
        )
    }
}

/// Read a text list into owned strings.
fn texts(list: capnp::text_list::Reader<'_>) -> capnp::Result<Vec<String>> {
    list.iter().map(|t| Ok(t?.to_string()?)).collect()
}

/// Copy strings into a text list of the same length.
fn set_texts(mut list: capnp::text_list::Builder<'_>, values: &[String]) {
    for (i, v) in values.iter().enumerate() {
        list.set(u32::try_from(i).unwrap(), v.as_str());
    }
}

/// RPC server of [`FakeGrantHost`].
struct FakeServer(Rc<FakeGrantHost>);

impl network_proxy::Server for FakeServer {
    async fn request_grant(
        self: Rc<Self>,
        params: network_proxy::RequestGrantParams,
        mut results: network_proxy::RequestGrantResults,
    ) -> Result<(), capnp::Error> {
        let req = params.get()?.get_req()?;
        let call = GrantCall {
            cmd: req.get_cmd()?.to_string()?,
            hosts: texts(req.get_hosts()?)?,
            rule: req.get_rule()?.to_string()?,
            env: texts(req.get_env()?)?,
            cwd: req.get_cwd()?.to_string()?,
        };
        let result = results.get().init_result();
        match self.0.decision {
            FakeDecision::Grant => {
                let mut grant = result.init_granted();
                let proxy = GrantProxy(self.0.clone());
                grant.set_proxy(capnp_rpc::new_client::<network_proxy::Client, _>(proxy));
                set_texts(
                    grant
                        .reborrow()
                        .init_hosts(u32::try_from(call.hosts.len()).unwrap()),
                    &call.hosts,
                );
                grant.set_duration_secs(FakeGrantHost::GRANT_SECS);
            }
            FakeDecision::DenyByUser => {
                let mut denied = result.init_denied();
                denied.set_by_user(true);
                denied.set_reason("denied by the user");
            }
            FakeDecision::DenyOther => {
                result.init_denied().set_reason(FakeGrantHost::DENY_REASON);
            }
        }
        self.0.calls.borrow_mut().push(call);
        Ok(())
    }

    async fn list_rules(
        self: Rc<Self>,
        _params: network_proxy::ListRulesParams,
        mut results: network_proxy::ListRulesResults,
    ) -> Result<(), capnp::Error> {
        let rules = &self.0.rules;
        let mut list = results
            .get()
            .init_rules(u32::try_from(rules.len()).unwrap());
        for (i, (name, description, allow, deny)) in rules.iter().enumerate() {
            let mut r = list.reborrow().get(u32::try_from(i).unwrap());
            r.set_name(name.as_str());
            r.set_enabled("on-demand");
            r.set_description(description.as_str());
            set_texts(
                r.reborrow().init_allow(u32::try_from(allow.len()).unwrap()),
                allow,
            );
            set_texts(r.init_deny(u32::try_from(deny.len()).unwrap()), deny);
        }
        Ok(())
    }

    async fn upsert_rule(
        self: Rc<Self>,
        params: network_proxy::UpsertRuleParams,
        mut results: network_proxy::UpsertRuleResults,
    ) -> Result<(), capnp::Error> {
        let r = params.get()?.get_rule()?;
        self.0.upserts.borrow_mut().push(FakeRuleChange {
            name: r.get_name()?.to_string()?,
            enabled: r.get_enabled()?.to_string()?,
            description: if r.get_set_description() {
                Some(r.get_description()?.to_string()?)
            } else {
                None
            },
            allow: if r.get_set_allow() {
                Some(texts(r.get_allow()?)?)
            } else {
                None
            },
            deny: if r.get_set_deny() {
                Some(texts(r.get_deny()?)?)
            } else {
                None
            },
        });
        let mut result = results.get().init_result();
        match &self.0.upsert {
            FakeUpsert::Approve => result.set_approved(()),
            FakeUpsert::DenyByUser => result.init_denied().set_by_user(true),
            FakeUpsert::Error(e) => result.set_error(e.as_str()),
        }
        Ok(())
    }
}

/// Grant proxy of [`FakeGrantHost`]. Serves the configured grant log and
/// the configured abort, and counts its release on drop.
struct GrantProxy(Rc<FakeGrantHost>);

impl network_proxy::Server for GrantProxy {
    async fn grant_log(
        self: Rc<Self>,
        _params: network_proxy::GrantLogParams,
        mut results: network_proxy::GrantLogResults,
    ) -> Result<(), capnp::Error> {
        let log = &self.0.log;
        let mut list = results
            .get()
            .init_entries(u32::try_from(log.len()).unwrap());
        for (i, (host, port, blocked)) in log.iter().enumerate() {
            let mut e = list.reborrow().get(u32::try_from(i).unwrap());
            e.set_host(host.as_str());
            e.set_port(*port);
            e.set_blocked(*blocked);
        }
        Ok(())
    }

    async fn wait_revoked(
        self: Rc<Self>,
        _params: network_proxy::WaitRevokedParams,
        mut results: network_proxy::WaitRevokedResults,
    ) -> Result<(), capnp::Error> {
        let abort_after = self.0.abort_after;
        // An open call must not keep the proxy alive, so that the test sees
        // the release.
        drop(self);
        match abort_after {
            Some(delay) => {
                tokio::time::sleep(delay).await;
                results.get().set_aborted(true);
                Ok(())
            }
            None => std::future::pending().await,
        }
    }
}

impl Drop for GrantProxy {
    fn drop(&mut self) {
        self.0.released.set(self.0.released.get() + 1);
    }
}

/// Serve the admin routes with the `airlock` MCP server on a local port.
/// Grant requests go to `host`.
/// Args:
///  - `state`: Shared admin state
///  - `host`: Fake host for the grant requests
///  - `booted`: `false` to test calls before the boot is complete
///  - `caps`: Agent features that the host enabled
pub(crate) async fn serve_admin_mcp(
    state: Arc<AdminState>,
    host: &Rc<FakeGrantHost>,
    booted: bool,
    caps: Capabilities,
) -> SocketAddr {
    let creds = SpawnCreds::default();
    if booted {
        *creds.borrow_mut() = Some((1000, 1000, false));
    }
    let mut ctx = GrantContext::new(
        host.client(),
        GrantRoutes::new(),
        creds,
        vec![FakeGrantHost::BASE_ENV.into()],
        FakeGrantHost::BASE_CWD.into(),
    );
    ctx.skip_netns = true;
    let grants = GrantBridge::start(ctx);
    serve(router(state, Some(grants), caps)).await
}

/// Send one JSON-RPC message to the admin MCP endpoint, as an MCP client
/// does. More header lines, for example `Origin`, go in `extra`.
pub(crate) async fn post_admin_mcp(addr: SocketAddr, body: &Value, extra: &[&str]) -> HttpReply {
    let mut headers = vec![
        "Host: admin.airlock",
        "Content-Type: application/json",
        "Accept: application/json, text/event-stream",
    ];
    headers.extend_from_slice(extra);
    http(addr, "POST", "/mcp", &headers, Some(body)).await
}

/// Call the tool `name` of the `airlock` MCP server and return the
/// JSON-RPC `result`.
pub(crate) async fn call_tool(addr: SocketAddr, name: &str, args: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": args },
    });
    let reply = post_admin_mcp(addr, &body, &[]).await;
    assert_eq!(reply.status, 200, "{reply:?}");
    let value: Value = serde_json::from_str(&reply.body).unwrap();
    value["result"].clone()
}
