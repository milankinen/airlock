//! Link between the MCP tool handlers and the host network service.
//!
//! One local task owns the host connection. The tool handlers run on other
//! tasks and send their operations to it. The operations are:
//!  * Execute: ask the host for a grant, run the command in an isolated
//!    network with the grant, stop the command if the host ends the grant,
//!    read the hosts that the command connected to, and then release the
//!    grant
//!  * Show the network rules that the sandbox can see
//!  * Ask the user to add or change a network rule

use std::cell::Cell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::network_capnp::{grant_result, network_proxy, upsert_result};
use rmcp::schemars;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use crate::net::{GrantNet, GrantRoutes};
use crate::process::{CollectSpec, spawn_collect};

/// Error text when the grant task is gone.
const STOPPED: &str = "The grant service stopped.";

/// Error text when the grant task gives a result of the wrong operation.
const MISMATCH: &str = "The grant service gave a wrong result.";

/// Number of bytes to keep from the end of stdout, and the same for stderr.
const OUTPUT_LIMIT: usize = 64 * 1024;

/// Credentials of the sandbox processes: `(uid, gid, harden)`. The value is
/// `None` until the boot succeeds.
pub type SpawnCreds = Rc<std::cell::RefCell<Option<(u32, u32, bool)>>>;

/// What the grant task needs from the boot. All values are `!Send`.
pub struct GrantContext {
    /// `NetworkProxy` client of the network channel. It serves
    /// `requestGrant`, `listRules` and `upsertRule`.
    pub network: network_proxy::Client,
    /// Map from grant source address to grant proxy.
    pub routes: GrantRoutes,
    /// Credentials of the sandbox processes.
    pub creds: SpawnCreds,
    /// Base environment of the commands (`KEY=VALUE`), as `airlock exec`
    /// uses it.
    pub env: Vec<String>,
    /// Default working directory of the commands.
    pub cwd: String,
    /// Test only: run the command with no network namespace, as the test
    /// user.
    #[cfg(test)]
    pub skip_netns: bool,
}

impl GrantContext {
    /// Create the context. See the fields for the arguments.
    pub fn new(
        network: network_proxy::Client,
        routes: GrantRoutes,
        creds: SpawnCreds,
        env: Vec<String>,
        cwd: String,
    ) -> Self {
        Self {
            network,
            routes,
            creds,
            env,
            cwd,
            #[cfg(test)]
            skip_netns: false,
        }
    }
}

/// Checked arguments of one `execute` call.
#[derive(Debug)]
pub struct ExecRequest {
    /// Shell command.
    pub cmd: String,
    /// More environment variables (`KEY=VALUE`).
    pub env: Vec<String>,
    /// Working directory, or `None` for the default.
    pub cwd: Option<String>,
    /// Requested hosts. Empty if `rule` is set.
    pub hosts: Vec<String>,
    /// Name of the on-demand rule. Empty if `hosts` is set.
    pub rule: String,
}

/// Result of one `execute` call.
#[derive(Debug, Default, Serialize, schemars::JsonSchema)]
pub struct ExecOutput {
    /// True if the user approved the request.
    pub approved: bool,
    /// Exit code of the command, or `None` if it did not exit normally.
    pub exit_code: Option<i32>,
    /// End of the standard output.
    pub stdout: String,
    /// End of the standard error.
    pub stderr: String,
    /// Hosts that the command connected to.
    pub hosts: Vec<HostEntry>,
    /// True if the user stopped the command.
    pub aborted: bool,
}

/// One connect target of a command.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct HostEntry {
    /// Target as `host:port`. An IPv6 address has brackets.
    pub host: String,
    /// True if the sandbox blocked the connection.
    pub blocked: bool,
}

/// One network rule, as `show_rules` gives it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Rule {
    /// Name of the rule.
    pub name: String,
    /// `true`, `false` or `"on-demand"`.
    pub enabled: Value,
    /// Description for the user.
    pub description: String,
    /// Allowed host patterns.
    pub allow: Vec<String>,
    /// Denied host patterns.
    pub deny: Vec<String>,
    /// True if the rule relays its hosts with no interception.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub passthrough: bool,
    /// True if the rule injects secrets.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub inject: bool,
}

/// A change of a network rule. `None` values do not change.
#[derive(Debug)]
pub struct RuleChange {
    /// Name of the rule.
    pub name: String,
    /// `true`, `false` or `on-demand`, or `None` to keep the value.
    pub enabled: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// New allowed host patterns.
    pub allow: Option<Vec<String>>,
    /// New denied host patterns.
    pub deny: Option<Vec<String>>,
}

/// One operation of the grant task.
enum Op {
    Execute(ExecRequest),
    ListRules,
    UpsertRule(RuleChange),
}

/// One request to the grant task.
struct Job {
    op: Op,
    reply: Reply,
}

/// Result of one operation of the grant task.
enum Output {
    Execute(ExecOutput),
    Rules(Vec<Rule>),
    Upsert(bool),
}

/// Reply channel of one job. An error is a text for the agent.
type Reply = oneshot::Sender<Result<Output, String>>;

/// Handle to the grant task. The handlers can share it between threads.
#[derive(Clone)]
pub struct GrantBridge {
    tx: mpsc::UnboundedSender<Job>,
}

impl GrantBridge {
    /// Start the grant task. Call inside a `LocalSet`.
    /// Args:
    ///  - `ctx`: Host connection, routes and credentials
    pub fn start(ctx: GrantContext) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let ctx = Rc::new(ctx);
        tokio::task::spawn_local(async move {
            while let Some(job) = rx.recv().await {
                tokio::task::spawn_local(run_job(ctx.clone(), job));
            }
        });
        Self { tx }
    }

    /// Run one command with a grant and wait for the result.
    ///
    /// Returns:
    ///   Command result, or an error text for the agent.
    pub async fn execute(&self, req: ExecRequest) -> Result<ExecOutput, String> {
        match self.send(Op::Execute(req)).await? {
            Output::Execute(out) => Ok(out),
            _ => Err(MISMATCH.into()),
        }
    }

    /// Get the network rules that the sandbox can see.
    ///
    /// Returns:
    ///   Rules, or an error text for the agent.
    pub async fn list_rules(&self) -> Result<Vec<Rule>, String> {
        match self.send(Op::ListRules).await? {
            Output::Rules(rules) => Ok(rules),
            _ => Err(MISMATCH.into()),
        }
    }

    /// Ask the user to add or change a network rule.
    ///
    /// Returns:
    ///   True if the user approved, or an error text for the agent.
    pub async fn upsert_rule(&self, rule: RuleChange) -> Result<bool, String> {
        match self.send(Op::UpsertRule(rule)).await? {
            Output::Upsert(approved) => Ok(approved),
            _ => Err(MISMATCH.into()),
        }
    }

    /// Send one operation to the grant task and wait for the result.
    async fn send(&self, op: Op) -> Result<Output, String> {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Job { op, reply }).is_err() {
            return Err(STOPPED.into());
        }
        rx.await.unwrap_or_else(|_| Err(STOPPED.into()))
    }
}

/// Run one job and send the result. If the client disconnects, stop the
/// work.
async fn run_job(ctx: Rc<GrantContext>, mut job: Job) {
    let result = match job.op {
        Op::Execute(req) => execute(&ctx, req, &mut job.reply)
            .await
            .map(Output::Execute),
        Op::ListRules => list_rules(&ctx).await.map(Output::Rules),
        Op::UpsertRule(rule) => upsert_rule(&ctx, &rule, &mut job.reply)
            .await
            .map(Output::Upsert),
    };
    let _ = job.reply.send(result);
}

/// Ask for the grant, run the command, and read the grant log.
///
/// Returns:
///   Command result, or an error text for the agent.
async fn execute(
    ctx: &GrantContext,
    req: ExecRequest,
    reply: &mut Reply,
) -> Result<ExecOutput, String> {
    let Some((uid, gid, harden)) = *ctx.creds.borrow() else {
        return Err("Command execution is not available before the sandbox starts.".into());
    };
    info!(
        "grant: request ({} host(s), rule {:?})",
        req.hosts.len(),
        req.rule
    );

    let cwd = req.cwd.unwrap_or_else(|| ctx.cwd.clone());
    let mut request = ctx.network.request_grant_request();
    {
        let mut r = request.get().init_req();
        r.set_cmd(req.cmd.as_str());
        r.set_rule(req.rule.as_str());
        r.set_cwd(cwd.as_str());
        set_texts(r.reborrow().init_hosts(len(&req.hosts)), &req.hosts);
        set_texts(r.init_env(len(&req.env)), &req.env);
    }
    // The approval can take minutes. Drop the request if the client goes
    // away, so the host can remove the approval request.
    let response = tokio::select! {
        r = request.send().promise => r.map_err(|e| format!("The grant request failed: {e}"))?,
        () = reply.closed() => return Err("The client disconnected.".into()),
    };
    let grant = read_grant(&response).map_err(|e| format!("The grant reply is not valid: {e}"))?;
    // The response holds a reference to the grant proxy. Drop it, so the
    // grant ends when the network ends.
    drop(response);
    let grant = match grant {
        Decision::Denied { by_user: true, .. } => {
            info!("grant: denied by the user");
            return Ok(ExecOutput::default());
        }
        Decision::Denied { reason, .. } => {
            info!("grant: denied: {reason}");
            return Err(format!("The request was denied: {reason}"));
        }
        Decision::Granted(grant) => grant,
    };
    info!(
        "grant: granted ({} host(s), {}s)",
        grant.hosts.len(),
        grant.duration_secs
    );

    // Keep a client of the grant proxy to read its log after the command.
    let log_client = grant.proxy.clone();
    let (net, netns) = grant_network(ctx, grant.proxy)
        .await
        .map_err(|e| format!("The grant network setup failed: {e:#}"))?;
    let spec = CollectSpec {
        cmd: req.cmd,
        env: command_env(
            &ca_defaults(std::path::Path::new(crate::sandbox_ns::ROOTFS)),
            ctx.env.clone(),
            req.env,
        ),
        cwd,
        netns,
        output_limit: OUTPUT_LIMIT,
    };
    let handle = spawn_collect(spec, uid, gid, harden)
        .map_err(|e| format!("The command did not start: {e:#}"))?;

    let limit = Duration::from_secs(u64::from(grant.duration_secs));
    let aborted = Cell::new(false);
    let revoked = wait_revoked(&log_client, &aborted);
    let cancel = async {
        tokio::select! {
            () = tokio::time::sleep(limit) => {}
            () = reply.closed() => {}
            () = revoked => {}
        }
    };
    let output = handle.wait(cancel).await;
    let aborted = aborted.get();
    // Read the log while the grant is still active. A failure does not
    // hide the output of the command.
    let hosts = grant_log(&log_client).await.unwrap_or_else(|e| {
        warn!("grant: log read failed: {e}");
        Vec::new()
    });
    // Release the grant: the routes and the proxy go away with the network
    // and the last client.
    drop(log_client);
    drop(net);
    let output = output.map_err(|e| format!("The command failed: {e:#}"))?;
    info!(
        "grant: done (exit {:?}, signal {:?}, killed {}, truncated {}/{})",
        output.exit_code,
        output.signal,
        output.killed,
        output.stdout_truncated,
        output.stderr_truncated
    );
    Ok(ExecOutput {
        approved: true,
        exit_code: output.exit_code,
        stdout: output.stdout,
        stderr: output.stderr,
        hosts,
        aborted,
    })
}

/// Wait until the host ends the grant.
/// Args:
///  - `proxy`: Grant proxy from the host
///  - `aborted`: Set to `true` if the user aborted the grant
///
/// Returns:
///   Future that completes when the host ends the grant. If the call fails,
///   the future does not complete.
fn wait_revoked<'a>(
    proxy: &network_proxy::Client,
    aborted: &'a Cell<bool>,
) -> impl Future<Output = ()> + 'a {
    // Send the call now, so the host can end the grant at any time.
    let promise = proxy.wait_revoked_request().send().promise;
    async move {
        let read = async { Ok::<_, capnp::Error>(promise.await?.get()?.get_aborted()) };
        match read.await {
            Ok(by_user) => {
                info!("grant: revoked by the host (aborted {by_user})");
                aborted.set(by_user);
            }
            // An old host or a failed call must not stop the command.
            Err(e) => {
                warn!("grant: revoke wait failed: {e}");
                std::future::pending::<()>().await;
            }
        }
    }
}

/// Read the connect targets of a grant.
///
/// Returns:
///   One entry for each target.
async fn grant_log(proxy: &network_proxy::Client) -> capnp::Result<Vec<HostEntry>> {
    let response = proxy.grant_log_request().send().promise.await?;
    let mut hosts = Vec::new();
    for e in response.get()?.get_entries()? {
        let host = e.get_host()?.to_str()?;
        // An IPv6 literal needs brackets before the port.
        let target = if host.contains(':') {
            format!("[{host}]:{}", e.get_port())
        } else {
            format!("{host}:{}", e.get_port())
        };
        hosts.push(HostEntry {
            host: target,
            blocked: e.get_blocked(),
        });
    }
    Ok(hosts)
}

/// Get the rules that the sandbox can see from the host.
///
/// Returns:
///   Rules, or an error text.
async fn list_rules(ctx: &GrantContext) -> Result<Vec<Rule>, String> {
    let read = async {
        let response = ctx.network.list_rules_request().send().promise.await?;
        response
            .get()?
            .get_rules()?
            .iter()
            .map(read_rule)
            .collect::<capnp::Result<Vec<_>>>()
    };
    read.await
        .map_err(|e| format!("The rule request failed: {e}"))
}

/// Ask the user to add or change a rule.
///
/// Returns:
///   True if the user approved, or an error text.
async fn upsert_rule(
    ctx: &GrantContext,
    rule: &RuleChange,
    reply: &mut Reply,
) -> Result<bool, String> {
    info!("rule: upsert request for {:?}", rule.name);
    let mut request = ctx.network.upsert_rule_request();
    {
        let mut r = request.get().init_rule();
        r.set_name(rule.name.as_str());
        r.set_enabled(rule.enabled.as_deref().unwrap_or_default());
        if let Some(description) = &rule.description {
            r.set_description(description.as_str());
            r.set_set_description(true);
        }
        if let Some(allow) = &rule.allow {
            r.set_set_allow(true);
            set_texts(r.reborrow().init_allow(len(allow)), allow);
        }
        if let Some(deny) = &rule.deny {
            r.set_set_deny(true);
            set_texts(r.init_deny(len(deny)), deny);
        }
    }
    // The approval can take minutes, as for the grant request.
    let response = tokio::select! {
        r = request.send().promise => r.map_err(|e| format!("The rule request failed: {e}"))?,
        () = reply.closed() => return Err("The client disconnected.".into()),
    };
    let read = || -> capnp::Result<Result<bool, String>> {
        Ok(match response.get()?.get_result()?.which()? {
            upsert_result::Approved(()) => Ok(true),
            upsert_result::Denied(d) => {
                let d = d?;
                if d.get_by_user() {
                    Ok(false)
                } else {
                    Err(format!(
                        "The request was denied: {}",
                        d.get_reason()?.to_str()?
                    ))
                }
            }
            upsert_result::Error(e) => Err(format!("The rule is not valid: {}", e?.to_str()?)),
        })
    };
    let approved = read().map_err(|e| format!("The rule reply is not valid: {e}"))??;
    info!("rule: {:?} approved {approved}", rule.name);
    Ok(approved)
}

/// Isolated network of a grant. Drop releases the grant.
enum GrantHold {
    /// Network namespace with the grant routes.
    Net(#[expect(dead_code, reason = "kept only for its drop")] GrantNet),
    /// Test only: no network namespace, only the grant proxy.
    #[cfg(test)]
    Proxy(#[expect(dead_code, reason = "kept only for its drop")] network_proxy::Client),
}

/// Create the isolated network of a grant.
/// Args:
///  - `ctx`: Grant context with the routes
///  - `proxy`: Grant proxy from the host
///
/// Returns:
///   The network, and the network namespace for the command.
async fn grant_network(
    ctx: &GrantContext,
    proxy: network_proxy::Client,
) -> anyhow::Result<(GrantHold, Option<OwnedFd>)> {
    // A network namespace needs root. Tests use the network of the test
    // and keep only the proxy until the command ends.
    #[cfg(test)]
    if ctx.skip_netns {
        return Ok((GrantHold::Proxy(proxy), None));
    }
    let net = GrantNet::create(&ctx.routes, proxy).await?;
    info!("grant: network at {}", net.addr());
    let netns = net.netns_fd().try_clone_to_owned()?;
    Ok((GrantHold::Net(net), Some(netns)))
}

/// Grant decision of the host, as owned values.
enum Decision {
    Granted(Grant),
    Denied { by_user: bool, reason: String },
}

/// Approved grant.
struct Grant {
    proxy: network_proxy::Client,
    hosts: Vec<String>,
    duration_secs: u32,
}

/// Read the `requestGrant` response into owned values.
fn read_grant(
    response: &capnp::capability::Response<network_proxy::request_grant_results::Owned>,
) -> capnp::Result<Decision> {
    match response.get()?.get_result()?.which()? {
        grant_result::Granted(g) => {
            let g = g?;
            Ok(Decision::Granted(Grant {
                proxy: g.get_proxy()?,
                hosts: texts(g.get_hosts()?)?,
                duration_secs: g.get_duration_secs(),
            }))
        }
        grant_result::Denied(d) => {
            let d = d?;
            Ok(Decision::Denied {
                by_user: d.get_by_user(),
                reason: d.get_reason()?.to_string()?,
            })
        }
    }
}

/// Read one rule into owned values.
fn read_rule(r: airlock_common::network_capnp::rule_info::Reader<'_>) -> capnp::Result<Rule> {
    let enabled = match r.get_enabled()?.to_str()? {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        other => Value::String(other.to_string()),
    };
    Ok(Rule {
        name: r.get_name()?.to_string()?,
        enabled,
        description: r.get_description()?.to_string()?,
        allow: texts(r.get_allow()?)?,
        deny: texts(r.get_deny()?)?,
        passthrough: r.get_passthrough(),
        inject: r.get_inject(),
    })
}

/// Read a text list into owned strings.
fn texts(list: capnp::text_list::Reader<'_>) -> capnp::Result<Vec<String>> {
    list.iter().map(|t| Ok(t?.to_string()?)).collect()
}

/// Copy strings into a text list of the same length.
fn set_texts(mut list: capnp::text_list::Builder<'_>, values: &[String]) {
    for (i, v) in values.iter().enumerate() {
        list.set(u32::try_from(i).unwrap_or(u32::MAX), v.as_str());
    }
}

/// Get the length of a list for a Cap'n Proto builder.
fn len<T>(values: &[T]) -> u32 {
    u32::try_from(values.len()).unwrap_or(u32::MAX)
}

/// Paths of the system CA bundle in the container, in the order of the
/// search. The guest init adds the airlock CA to this bundle.
const CA_BUNDLES: [&str; 4] = [
    // Debian, Ubuntu and Alpine.
    "/etc/ssl/certs/ca-certificates.crt",
    // Fedora and RHEL.
    "/etc/pki/tls/certs/ca-bundle.crt",
    // openSUSE.
    "/etc/ssl/ca-bundle.pem",
    // Other systems, for example Arch with a link to the bundle.
    "/etc/ssl/cert.pem",
];

/// Directory of the hashed system CA certificates, if the image has it.
const CA_DIR: &str = "/etc/ssl/certs";

/// Variables that make common tools use the system CA bundle, so that TLS
/// through the airlock proxy works. Each one gets the bundle path. All of
/// them replace the store of the tool with the system bundle, which has
/// the public CAs too, or add the bundle to the store.
const CA_FILE_VARS: [&str; 12] = [
    // OpenSSL, Python `ssl`, Go, Ruby and many other tools.
    "SSL_CERT_FILE",
    // Python `requests`.
    "REQUESTS_CA_BUNDLE",
    // curl and libcurl.
    "CURL_CA_BUNDLE",
    // Node.js: adds the bundle to the built-in CAs.
    "NODE_EXTRA_CA_CERTS",
    // pip.
    "PIP_CERT",
    // Deno: adds the bundle to the store.
    "DENO_CERT",
    // git over HTTPS.
    "GIT_SSL_CAINFO",
    // AWS CLI and SDKs.
    "AWS_CA_BUNDLE",
    // cargo.
    "CARGO_HTTP_CAINFO",
    // Elixir hex.
    "HEX_CACERTS_PATH",
    // Nix.
    "NIX_SSL_CERT_FILE",
    // npm, as the `cafile` setting.
    "npm_config_cafile",
];

/// Get the CA defaults of a command: the variables of [`CA_FILE_VARS`]
/// with the system CA bundle, `SSL_CERT_DIR` if the image has that
/// directory, and `UV_NATIVE_TLS=1` (uv then uses the system store).
/// Args:
///  - `root`: Container root as this process sees it
///
/// Returns:
///   `KEY=VALUE` entries with container paths. Empty if the image has no
///   known CA bundle.
fn ca_defaults(root: &std::path::Path) -> Vec<String> {
    let in_root = |path: &str| crate::util::resolve_in_root(root, path);
    let Some(bundle) = CA_BUNDLES.iter().find(|p| in_root(p).is_file()) else {
        return Vec::new();
    };
    let mut env: Vec<String> = CA_FILE_VARS
        .iter()
        .map(|key| format!("{key}={bundle}"))
        .collect();
    if in_root(CA_DIR).is_dir() {
        env.push(format!("SSL_CERT_DIR={CA_DIR}"));
    }
    env.push("UV_NATIVE_TLS=1".into());
    env
}

/// Build the environment of a command. A later layer replaces a value of
/// an earlier layer with the same key.
/// Args:
///  - `defaults`: CA defaults (see [`ca_defaults`])
///  - `sandbox`: Base environment of the sandbox, as `airlock exec` uses it
///  - `agent`: Environment from the `execute` call. Its keys are checked
///    before
fn command_env(defaults: &[String], sandbox: Vec<String>, agent: Vec<String>) -> Vec<String> {
    merge_env(merge_env(defaults.to_vec(), sandbox), agent)
}

/// Add the agent's environment to the base environment. An agent value
/// replaces a base value with the same key.
fn merge_env(base: Vec<String>, extra: Vec<String>) -> Vec<String> {
    let key = |e: &str| e.split_once('=').map_or(e, |(k, _)| k).to_string();
    let extra_keys: Vec<String> = extra.iter().map(|e| key(e)).collect();
    base.into_iter()
        .filter(|e| !extra_keys.contains(&key(e)))
        .chain(extra)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that the CA defaults use the first CA bundle that the container
    /// root has, and that a root with no bundle gets no defaults. The check
    /// must look in the container root, not in the root of airlockd.
    ///   1. Make a temp root with no bundle and check that no default is set
    ///   2. Add the Fedora bundle and check the paths
    ///   3. Add the Debian bundle and its directory, and check that it wins
    #[test]
    fn ca_defaults_use_first_bundle_in_container_root() {
        let root = crate::test_cfg::temp_dir();
        assert!(ca_defaults(root.path()).is_empty());

        let add = |path: &str| {
            let file = root.path().join(path.trim_start_matches('/'));
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "").unwrap();
        };
        add("/etc/pki/tls/certs/ca-bundle.crt");
        let env = ca_defaults(root.path());
        assert!(env.contains(&"SSL_CERT_FILE=/etc/pki/tls/certs/ca-bundle.crt".to_string()));
        assert!(env.contains(&"UV_NATIVE_TLS=1".to_string()));
        assert!(!env.iter().any(|e| e.starts_with("SSL_CERT_DIR=")));

        add("/etc/ssl/certs/ca-certificates.crt");
        let env = ca_defaults(root.path());
        assert!(
            env.contains(&"NODE_EXTRA_CA_CERTS=/etc/ssl/certs/ca-certificates.crt".to_string())
        );
        assert!(env.contains(&"SSL_CERT_DIR=/etc/ssl/certs".to_string()));
    }

    /// Test the order of the environment layers: the CA defaults, then the
    /// sandbox env, then the env of the agent. A later layer wins.
    ///   1. Give the same keys in more layers
    ///   2. Check the value of each key and that each key shows once
    #[test]
    fn command_env_layers_defaults_sandbox_then_agent() {
        let strings = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        let env = command_env(
            &strings(&["SSL_CERT_FILE=/ca", "PIP_CERT=/ca", "UV_NATIVE_TLS=1"]),
            strings(&["PIP_CERT=/sandbox", "MODE=sandbox", "PATH=/bin"]),
            strings(&["MODE=agent"]),
        );
        let mut env = env;
        env.sort();
        assert_eq!(
            env,
            strings(&[
                "MODE=agent",
                "PATH=/bin",
                "PIP_CERT=/sandbox",
                "SSL_CERT_FILE=/ca",
                "UV_NATIVE_TLS=1",
            ])
        );
    }
}
