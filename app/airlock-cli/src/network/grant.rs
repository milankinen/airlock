//! Temporary network access and on-demand rules.
//!
//! A process in the sandbox can ask to run one command with access to more
//! hosts, or with an on-demand rule of the config. The user approves or
//! denies each request on the host. A granted request gets its own proxy,
//! which allows the approved hosts or applies the rule in addition to the
//! normal policy. Config deny rules, local hosts and the hosts of network
//! services still stay denied. The proxy records the hosts that the command
//! connects to, and if the network blocked them.
//!
//! The access ends when the sandbox releases the grant proxy, when the
//! duration expires, or when the guest connection is lost. All open
//! connections of the grant then close.
//!
//! A request for all hosts allows all public hosts that a grant can allow.
//!
//! The sandbox can also list the network rules, and ask to add or change
//! one. The config sets which rules the sandbox can see and change. An
//! approved change applies at once to new connections and goes into the
//! project config file. An on-demand rule stays inactive until the user
//! approves a command with it.
//!
//! Only one request per sandbox waits for the user at a time. A small
//! number of other requests can wait in a queue. When the queue is full,
//! new requests are denied.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use airlock_common::network_capnp::{connect_target, denied, network_proxy, rule_info};
use anyhow::Context as _;
use tokio::sync::Semaphore;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::matchers::canonical_host;
use super::rule_file::{self, ApprovedRules, RuleWrite};
use super::rules::{OnDemandRule, parse_pattern};
use super::target::{InjectedSecret, NetworkTarget, ip_literal, is_public_ip};
use super::{Network, add_secrets};
use crate::approval::{
    Approval, Approver, Decision, GrantPrompt, PromptCommand, PromptRequest, Running,
};
use crate::config::config_values::{Grants, NetworkRule, Policy, RuleEditing, RuleEnabled};
use crate::messages::grants as msg;
use crate::project::Project;
use crate::util::VerifiedPath;

/// Maximum number of host patterns in one request, or of new patterns in
/// one rule list.
const MAX_HOSTS: usize = 32;

/// Maximum number of patterns in one rule list after a change.
const MAX_RULE_PATTERNS: usize = 256;

/// Host of a request for all hosts.
const ALL_HOSTS: &str = "*";

/// Maximum length of a rule name.
const MAX_RULE_NAME: usize = 64;

/// Maximum number of entries in the connection log of one grant.
const MAX_LOG: usize = 256;

/// Maximum number of requests that wait behind the open request.
const MAX_WAITING: usize = 4;

/// Extra wait time after the approval timeout. The approver must stop at
/// the timeout. This wait is a backstop.
const APPROVAL_GRACE: Duration = Duration::from_secs(5);

/// Time after a user deny when new requests are denied with no prompt.
const DENY_COOLDOWN: Duration = Duration::from_secs(30);

/// Deny text for requests in the deny cooldown.
const COOLDOWN_TEXT: &str = "the user denied a request less than 30 seconds ago. \
                             Wait, then ask again only if necessary.";

/// Maximum length of the command in the log.
const LOG_CMD_CHARS: usize = 200;

/// Maximum length of a host name and of one label (RFC 1035).
const MAX_NAME_LEN: usize = 253;
const MAX_LABEL_LEN: usize = 63;

/// Name suffixes of local and private networks.
const LOCAL_SUFFIXES: &[&str] = &[
    "localhost",
    "local",
    "lan",
    "internal",
    "home.arpa",
    "airlock",
];

#[cfg(test)]
thread_local! {
    /// Accept loopback IP literals in grants on this thread. Only tests set
    /// it, because their upstream servers listen on loopback.
    static LOOPBACK_OK: Cell<bool> = const { Cell::new(false) };
}

/// Accept loopback IP literals in grants on this thread, for test
/// upstream servers. The grant connections then can also go to local
/// addresses.
#[cfg(test)]
pub(crate) fn allow_loopback_on_this_thread() {
    LOOPBACK_OK.set(true);
}

/// Refuse loopback IP literals in grants on this thread again.
#[cfg(test)]
pub(crate) fn deny_loopback_on_this_thread() {
    LOOPBACK_OK.set(false);
}

/// Return true if a connection that only a grant allows must go to public
/// addresses only. This is always true, except in tests that use loopback
/// upstream servers.
pub(crate) fn grant_needs_public_only() -> bool {
    #[cfg(test)]
    if LOOPBACK_OK.get() {
        return false;
    }
    true
}

/// Data that a grant and a rule change need from the booted sandbox.
pub struct GrantContext {
    /// Name of the sandbox, for the approval prompt.
    pub sandbox: String,
    /// Asks the user for a decision.
    pub approver: Rc<dyn Approver>,
    /// Project root directory. An approved rule change goes into its
    /// config file.
    pub project_dir: VerifiedPath,
    /// Sandbox directory. An approved rule change also goes into its copy
    /// of the config of the last run.
    pub sandbox_dir: VerifiedPath,
    /// Rule tables of the project files that the user approved. An
    /// approved rule change updates them.
    approved_rules: RefCell<ApprovedRules>,
}

impl GrantContext {
    /// Make the context.
    /// Args:
    ///  - `sandbox`: Name of the sandbox, for the approval prompt
    ///  - `approver`: Asks the user for a decision
    ///  - `project_dir`: Project root directory
    ///  - `sandbox_dir`: Sandbox directory with the config of the last run.
    ///    The approved rule tables come from that config (see
    ///    [`ApprovedRules::load`]).
    pub fn new(
        sandbox: String,
        approver: Rc<dyn Approver>,
        project_dir: VerifiedPath,
        sandbox_dir: VerifiedPath,
    ) -> Self {
        // With no approved tables, a change of a rule that a project file
        // has is refused. A new rule is still possible.
        let approved = ApprovedRules::load(&sandbox_dir, &project_dir).unwrap_or_else(|e| {
            warn!("rule change: cannot read the approved rules: {e:#}");
            ApprovedRules::default()
        });
        Self {
            sandbox,
            approver,
            project_dir,
            sandbox_dir,
            approved_rules: RefCell::new(approved),
        }
    }

    /// Make the context of `project` with the system approver.
    /// Args:
    ///  - `project`: The locked project
    ///  - `monitor`: Place of the monitor TUI dialog. The approver uses it
    ///    when the host desktop cannot show the request.
    pub fn for_project(project: &Project, monitor: crate::approval::MonitorSlot) -> Self {
        Self::new(
            project.host_cwd.file_name().map_or_else(
                || project.host_cwd.to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
            crate::approval::system(&project.context.data, monitor),
            project.host_cwd.clone(),
            project.sandbox_dir.clone(),
        )
    }
}

/// Agent features that a sandbox network serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentAccess {
    /// The sandbox can ask for temporary access to hosts.
    pub grants: bool,
    /// The rules that the sandbox can see and change.
    pub rule_editing: RuleEditing,
}

/// Settings and state of the temporary grants of one sandbox network.
pub(crate) struct GrantState {
    enabled: bool,
    /// Rules that the sandbox can see and change.
    rule_editing: RuleEditing,
    approval_timeout: Duration,
    max_duration_secs: u32,
    context: Option<Rc<GrantContext>>,
    /// One permit: only one request waits for the user at a time.
    gate: Semaphore,
    /// Number of requests that wait for the user or for the gate.
    pending: Cell<usize>,
    /// End of the deny cooldown after the last user deny.
    cooldown_until: Cell<Option<Instant>>,
}

impl GrantState {
    /// Make the grant state from the `[network.grants]` config and the
    /// `network.rule_editing` value. It denies all requests until
    /// [`GrantState::attach`] is called.
    pub(crate) fn new(config: &Grants, rule_editing: RuleEditing) -> Self {
        Self {
            enabled: config.enabled,
            rule_editing,
            approval_timeout: Duration::from_secs(config.approval_timeout.into()),
            max_duration_secs: config.max_duration,
            context: None,
            gate: Semaphore::new(1),
            pending: Cell::new(0),
            cooldown_until: Cell::new(None),
        }
    }

    /// Attach the sandbox data. After this call, requests go to the user.
    pub(crate) fn attach(&mut self, context: GrantContext) {
        self.context = Some(Rc::new(context));
    }

    /// Get the agent features. All are off if no sandbox data is attached.
    pub(crate) fn access(&self) -> AgentAccess {
        AgentAccess {
            grants: self.enabled && self.context.is_some(),
            rule_editing: self.rule_editing(),
        }
    }

    /// Get the rules that the sandbox can see and change. It is
    /// [`RuleEditing::None`] if no sandbox data is attached.
    fn rule_editing(&self) -> RuleEditing {
        if self.context.is_some() {
            self.rule_editing
        } else {
            RuleEditing::None
        }
    }

    /// Return true if a user deny was less than [`DENY_COOLDOWN`] ago.
    fn in_cooldown(&self) -> bool {
        self.cooldown_until
            .get()
            .is_some_and(|until| Instant::now() < until)
    }
}

/// Decrements the pending request count when the request ends, also when
/// the guest cancels it.
struct PendingGuard<'a>(&'a Cell<usize>);

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

/// Return true if a grant can allow the destination `host`. The host must
/// pass the same checks as a host in a grant request: a public IP literal,
/// or a valid name that is not local, not a public suffix and not a
/// numeric address form.
pub(crate) fn is_grantable_host(host: &str) -> bool {
    check_host(&canonical_host(host)).is_ok()
}

/// Check that an IP literal in a grant is public.
fn check_ip(host: &str, ip: std::net::IpAddr) -> Result<(), String> {
    #[cfg(test)]
    if LOOPBACK_OK.get() && ip.is_loopback() {
        return Ok(());
    }
    if is_public_ip(ip) {
        Ok(())
    } else {
        Err(format!("{host}: local address"))
    }
}

/// Check one canonical host or `*.suffix` pattern of a grant request.
/// Returns:
///   Error text that tells why the host is refused.
fn check_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("empty host".into());
    }
    if let Some(ip) = ip_literal(host) {
        return check_ip(host, ip);
    }
    let (wildcard, name) = match host.strip_prefix("*.") {
        Some(base) => (true, base),
        None => (false, host),
    };
    if name.contains('*') {
        return Err(format!("{host}: wildcard is not supported"));
    }
    let labels: Vec<&str> = name.split('.').collect();
    let valid = name.len() <= MAX_NAME_LEN
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= MAX_LABEL_LEN
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        });
    if !valid {
        return Err(format!("{host}: not a valid host name"));
    }
    // The system resolver reads names such as `2130706433`, `127.1`,
    // `127.0x1` or `0x7f.1` as IP addresses. A real top-level domain has a
    // letter, and a host name label does not start with `0x`.
    let is_hex = |l: &str| l.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("0x"));
    let is_number = |l: &str| l.chars().all(|c| c.is_ascii_digit()) || is_hex(l);
    let last = labels[labels.len() - 1];
    if labels.iter().any(|l| is_hex(l))
        || labels.iter().all(|l| is_number(l))
        || !last.chars().any(|c| c.is_ascii_alphabetic())
    {
        return Err(format!("{host}: numeric address form"));
    }
    // A single label is an intranet name.
    if labels.len() < 2 {
        return Err(format!("{host}: single-label name"));
    }
    let local = LOCAL_SUFFIXES
        .iter()
        .any(|s| name == *s || name.ends_with(&format!(".{s}")));
    if local {
        return Err(format!("{host}: local or sandbox host"));
    }
    // A grant for a public suffix, or a wildcard on it, would allow all
    // domains under it (for example `*.co.uk` or `*.github.io`).
    if psl::suffix_str(name) == Some(name) {
        return Err(if wildcard {
            format!("{host}: wildcard is too wide")
        } else {
            format!("{host}: public suffix")
        });
    }
    Ok(())
}

/// Parse and check the host patterns of a grant request.
/// Args:
///  - `hosts`: Patterns from the request (`host`, `host:port` or
///    `*.suffix`)
///
/// Returns:
///   The patterns in canonical form with no duplicates, and the targets
///   for the policy. Error text if the list is empty, too long, or has a
///   refused pattern.
pub(crate) fn parse_hosts(hosts: &[String]) -> Result<(Vec<String>, Vec<NetworkTarget>), String> {
    if hosts.is_empty() {
        return Err("no hosts in the request".into());
    }
    if hosts.len() > MAX_HOSTS {
        return Err(format!("too many hosts (maximum {MAX_HOSTS})"));
    }
    let mut labels = Vec::new();
    let mut targets: Vec<NetworkTarget> = Vec::new();
    for raw in hosts {
        let raw = raw.trim();
        let (host, port) = parse_pattern(raw).map_err(|e| e.to_string())?;
        let host = canonical_host(host);
        check_host(&host)?;
        if port == Some(0) {
            return Err(format!("{raw}: port 0"));
        }
        if targets.iter().any(|t| t.host == host && t.port == port) {
            continue;
        }
        labels.push(match port {
            Some(p) if host.contains(':') => format!("[{host}]:{p}"),
            Some(p) => format!("{host}:{p}"),
            None => host.clone(),
        });
        targets.push(NetworkTarget { host, port });
    }
    Ok((labels, targets))
}

/// Return true if the network allows all `targets` with no grant. A
/// wildcard pattern always needs the grant. A pattern with no port is
/// checked on ports 80 and 443.
fn all_allowed_already(network: &Network, targets: &[NetworkTarget]) -> bool {
    targets.iter().all(|t| {
        if t.host.starts_with("*.") {
            return false;
        }
        let ports = t.port.map_or(vec![80, 443], |p| vec![p]);
        ports
            .into_iter()
            .all(|p| network.resolve_target_in(&t.host, p, None).allowed)
    })
}

/// Extra access of a grant connection.
pub(crate) enum GrantAccess {
    /// Host patterns that the user approved. They act as more allow rules.
    Hosts(Vec<NetworkTarget>),
    /// All hosts that a grant can allow (see [`is_grantable_host`]).
    AllHosts,
    /// On-demand rule that the user approved. It acts as an enabled rule.
    Rule(Rc<OnDemandRule>),
}

impl GrantAccess {
    /// Return true if the access allows `host:port`.
    pub(crate) fn allows(&self, host: &str, port: u16) -> bool {
        let targets = match self {
            GrantAccess::Hosts(targets) => targets,
            GrantAccess::AllHosts => return is_grantable_host(host),
            GrantAccess::Rule(rule) => &rule.targets.allow,
        };
        targets.iter().any(|t| t.matches(host, port))
    }

    /// Return true if the deny patterns of the rule match `host:port`.
    pub(crate) fn denies(&self, host: &str, port: u16) -> bool {
        match self {
            GrantAccess::Hosts(_) | GrantAccess::AllHosts => false,
            GrantAccess::Rule(rule) => rule.targets.deny.iter().any(|t| t.matches(host, port)),
        }
    }

    /// Return true if the rule relays `host:port` with no interception.
    pub(crate) fn passthrough(&self, host: &str, port: u16) -> bool {
        match self {
            GrantAccess::Hosts(_) | GrantAccess::AllHosts => false,
            GrantAccess::Rule(rule) => rule
                .targets
                .passthrough
                .iter()
                .any(|t| t.matches(host, port)),
        }
    }

    /// Add the secrets that the rule injects for `host:port` to `out`.
    pub(crate) fn add_secrets(&self, host: &str, port: u16, out: &mut Vec<InjectedSecret>) {
        if let GrantAccess::Rule(rule) = self {
            add_secrets(&rule.inject, host, port, out);
        }
    }
}

/// Why a request is denied.
#[derive(Debug)]
struct Denial {
    /// True if the user denied the request.
    by_user: bool,
    /// Text for the guest.
    reason: String,
}

impl From<String> for Denial {
    fn from(reason: String) -> Self {
        Self {
            by_user: false,
            reason,
        }
    }
}

impl From<&str> for Denial {
    fn from(reason: &str) -> Self {
        reason.to_string().into()
    }
}

/// Write a deny into a `Denied` struct of a result.
fn set_denied(mut out: denied::Builder<'_>, denial: &Denial) {
    out.set_by_user(denial.by_user);
    out.set_reason(&denial.reason);
}

/// Read a capnp text list.
fn text_list(list: capnp::text_list::Reader<'_>) -> Result<Vec<String>, capnp::Error> {
    list.iter().map(|t| Ok(t?.to_str()?.to_string())).collect()
}

/// Write strings into a capnp text list.
fn set_texts(mut list: capnp::text_list::Builder<'_>, items: &[String]) {
    for (i, item) in items.iter().enumerate() {
        list.set(i as u32, item);
    }
}

/// One grant request from the guest.
struct Request {
    cmd: String,
    hosts: Vec<String>,
    rule: String,
    env: Vec<String>,
    cwd: String,
}

/// An approved grant, ready to send to the guest.
struct Granted {
    proxy: network_proxy::Client,
    hosts: Vec<String>,
    duration_secs: u32,
}

/// Serve `NetworkProxy.requestGrant` of the default proxy.
/// Args:
///  - `network`: Network of the sandbox. The grant proxy shares it
///  - `params`, `results`: RPC parameters and results
///
/// Returns:
///   Ok with a granted or denied result. Error only if the request message
///   is not valid.
pub(super) async fn request(
    network: Rc<Network>,
    params: network_proxy::RequestGrantParams,
    mut results: network_proxy::RequestGrantResults,
) -> Result<(), capnp::Error> {
    let req = params.get()?.get_req()?;
    let request = Request {
        cmd: req.get_cmd()?.to_str()?.to_string(),
        hosts: text_list(req.get_hosts()?)?,
        rule: req.get_rule()?.to_str()?.to_string(),
        env: text_list(req.get_env()?)?,
        cwd: req.get_cwd()?.to_str()?.to_string(),
    };
    match decide(network, request).await {
        Ok(granted) => {
            info!(
                "grant: allowed [{}] for {} s",
                granted.hosts.join(", "),
                granted.duration_secs
            );
            let mut g = results.get().init_result().init_granted();
            g.set_proxy(granted.proxy);
            g.set_duration_secs(granted.duration_secs);
            set_texts(g.init_hosts(granted.hosts.len() as u32), &granted.hosts);
        }
        Err(denial) => {
            info!("grant: denied: {}", denial.reason);
            set_denied(results.get().init_result().init_denied(), &denial);
        }
    }
    Ok(())
}

/// Get the attached sandbox data.
/// Returns:
///   The sandbox data, or the deny if no sandbox data is attached.
fn sandbox_context(network: &Network) -> Result<Rc<GrantContext>, Denial> {
    network
        .grants
        .context
        .clone()
        .ok_or_else(|| "temporary network access is not available in this sandbox".into())
}

/// Check that the sandbox can run a command with more access.
/// Args:
///  - `network`: Network of the sandbox
///  - `rule`: `true` for a request with an on-demand rule, `false` for a
///    request with hosts
///
/// Returns:
///   The sandbox data, or the deny.
fn grant_context(network: &Network, rule: bool) -> Result<Rc<GrantContext>, Denial> {
    let state = &network.grants;
    // A host request needs `network.grants`. An on-demand rule needs
    // `network.rule_editing`. The two settings are independent.
    if rule {
        if state.rule_editing == RuleEditing::None {
            return Err("on-demand rules are disabled".into());
        }
    } else if !state.enabled {
        return Err("temporary network access is disabled in the config \
                    (network.grants.enabled)"
            .into());
    }
    let context = sandbox_context(network)?;
    if matches!(network.policy(), Policy::DenyAlways) {
        return Err("the network policy is deny-always".into());
    }
    Ok(context)
}

/// Check the request, ask the user, and make the grant proxy.
/// Returns:
///   The grant, or the deny.
async fn decide(network: Rc<Network>, req: Request) -> Result<Granted, Denial> {
    let cmd_log: String = req.cmd.chars().take(LOG_CMD_CHARS).collect();
    info!(
        "grant: request for [{}] rule {:?}, cmd {cmd_log:?}, {} env entries",
        req.hosts.join(", "),
        req.rule,
        req.env.len()
    );
    let context = grant_context(&network, req.hosts.is_empty() && !req.rule.is_empty())?;
    let duration_secs = network.grants.max_duration_secs;
    let command = PromptCommand {
        cwd: req.cwd,
        cmd: req.cmd,
        env: req.env,
        duration_secs,
    };
    let (access, hosts, prompt) = match (req.hosts.is_empty(), req.rule.is_empty()) {
        (false, true) if req.hosts.iter().any(|h| h.trim() == ALL_HOSTS) => {
            if req.hosts.len() > 1 {
                return Err("`*` must be the only host".into());
            }
            let hosts = vec![ALL_HOSTS.to_string()];
            let prompt = PromptRequest::Hosts {
                hosts: hosts.clone(),
                command,
            };
            (GrantAccess::AllHosts, hosts, prompt)
        }
        (false, true) => {
            let (hosts, targets) = parse_hosts(&req.hosts)?;
            if all_allowed_already(&network, &targets) {
                return Err("the network already allows all requested hosts. \
                            Run the command with no grant."
                    .into());
            }
            let prompt = PromptRequest::Hosts {
                hosts: hosts.clone(),
                command,
            };
            (GrantAccess::Hosts(targets), hosts, prompt)
        }
        (true, false) => {
            let rules = network.rules.borrow();
            let (Some(rule), Some(config)) = (
                rules.on_demand.get(&req.rule).cloned(),
                rules.rules().get(&req.rule).cloned(),
            ) else {
                return Err(format!("no on-demand rule {:?}", req.rule).into());
            };
            let hosts = rule.allow_patterns.clone();
            let prompt = PromptRequest::Rule {
                name: req.rule.clone(),
                rule: config,
                command,
            };
            (GrantAccess::Rule(rule), hosts, prompt)
        }
        _ => return Err("give either hosts or a rule".into()),
    };
    let running = ask_user(&network, &context, prompt).await?;

    let proxy = ScopedProxy::start(
        network.clone(),
        access,
        Duration::from_secs(duration_secs.into()),
        running,
    );
    Ok(Granted {
        proxy: capnp_rpc::new_client(proxy),
        hosts,
        duration_secs,
    })
}

/// Ask the user to approve a request. Only one request waits for the user
/// at a time. After a user deny, new requests are denied for a short time
/// with no prompt.
/// Returns:
///   Ok if the user approved, with the notice of the running command if the
///   host shows one. Else the deny.
async fn ask_user(
    network: &Network,
    context: &GrantContext,
    request: PromptRequest,
) -> Result<Option<Running>, Denial> {
    let state = &network.grants;
    if state.in_cooldown() {
        return Err(COOLDOWN_TEXT.into());
    }
    if state.pending.get() > MAX_WAITING {
        return Err("too many open requests for temporary network access".into());
    }
    state.pending.set(state.pending.get() + 1);
    let _pending = PendingGuard(&state.pending);
    // The semaphore is never closed. Thus the acquire does not fail.
    let _permit = state
        .gate
        .acquire()
        .await
        .map_err(|_| "approval queue is closed".to_string())?;
    // The user can deny while this request waits in the queue.
    if state.in_cooldown() {
        return Err(COOLDOWN_TEXT.into());
    }

    let prompt = GrantPrompt {
        sandbox: context.sandbox.clone(),
        request,
    };
    let timeout = state.approval_timeout;
    let approval = tokio::time::timeout(
        timeout + APPROVAL_GRACE,
        context.approver.approve(prompt, timeout),
    )
    .await
    .unwrap_or_else(|_| Approval::from(Decision::Deny("no answer from the user".into())));
    match approval.decision {
        Decision::Allow => Ok(approval.running),
        Decision::DeniedByUser => {
            // Do not show the user more prompts soon after a deny. A timeout
            // or a missing notification service is not a user deny.
            state
                .cooldown_until
                .set(Some(Instant::now() + DENY_COOLDOWN));
            Err(Denial {
                by_user: true,
                reason: msg::DENIED_BY_USER.into(),
            })
        }
        // A timeout or a dismissed prompt sets no cooldown on purpose. Then
        // the guest can show a new prompt at most once in each approval
        // timeout.
        Decision::Deny(why) => Err(why.into()),
    }
}

/// Serve `NetworkProxy.listRules` of the default proxy: the rules that
/// `network.rule_editing` lets the sandbox see.
pub(super) fn list_rules(network: &Network, mut results: network_proxy::ListRulesResults) {
    let editing = network.grants.rule_editing();
    let rules = network.rules.borrow();
    let shown: Vec<(&String, &NetworkRule)> = rules
        .rules()
        .iter()
        .filter(|(_, r)| match editing {
            RuleEditing::All => true,
            RuleEditing::OnDemand => r.enabled == RuleEnabled::OnDemand,
            RuleEditing::None => false,
        })
        .collect();
    let mut list = results.get().init_rules(shown.len() as u32);
    for (i, (name, rule)) in shown.into_iter().enumerate() {
        let mut out: rule_info::Builder<'_> = list.reborrow().get(i as u32);
        out.set_name(name);
        out.set_description(&rule.description);
        out.set_enabled(rule.enabled.as_text());
        out.set_passthrough(rule.passthrough);
        out.set_inject(!rule.inject.is_empty());
        set_texts(
            out.reborrow().init_allow(rule.allow.len() as u32),
            &rule.allow,
        );
        set_texts(out.init_deny(rule.deny.len() as u32), &rule.deny);
    }
}

/// A rule change from the sandbox. `None` values do not change.
#[derive(Clone, Debug)]
struct RuleChange {
    name: String,
    enabled: Option<RuleEnabled>,
    description: Option<String>,
    allow: Option<Vec<String>>,
    deny: Option<Vec<String>>,
}

/// Serve `NetworkProxy.upsertRule` of the default proxy.
/// Args:
///  - `network`: Network of the sandbox
///  - `params`, `results`: RPC parameters and results
///
/// Returns:
///   Ok with an approved, denied or error result. Error only if the
///   request message is not valid.
pub(super) async fn upsert_rule(
    network: Rc<Network>,
    params: network_proxy::UpsertRuleParams,
    mut results: network_proxy::UpsertRuleResults,
) -> Result<(), capnp::Error> {
    let rule = params.get()?.get_rule()?;
    let enabled = rule.get_enabled()?.to_str()?.trim().to_string();
    let parsed = RuleChange {
        name: rule.get_name()?.to_str()?.trim().to_string(),
        enabled: RuleEnabled::from_text(&enabled),
        description: if rule.get_set_description() {
            Some(rule.get_description()?.to_str()?.to_string())
        } else {
            None
        },
        allow: if rule.get_set_allow() {
            Some(text_list(rule.get_allow()?)?)
        } else {
            None
        },
        deny: if rule.get_set_deny() {
            Some(text_list(rule.get_deny()?)?)
        } else {
            None
        },
    };
    let result = if !enabled.is_empty() && parsed.enabled.is_none() {
        Err(RuleError::Invalid(format!(
            "enabled {enabled:?} is not valid: use true, false or \"on-demand\""
        )))
    } else {
        change_rule(&network, parsed).await
    };
    let mut out = results.get().init_result();
    match result {
        Ok(()) => out.set_approved(()),
        Err(RuleError::Denied(denial)) => {
            info!("rule change: denied: {}", denial.reason);
            set_denied(out.init_denied(), &denial);
        }
        Err(RuleError::Invalid(why)) => {
            info!("rule change: refused: {why}");
            out.set_error(&why);
        }
    }
    Ok(())
}

/// Why a rule change did not happen.
enum RuleError {
    /// The user or the config denied the change.
    Denied(Denial),
    /// The request is not valid, or the change failed.
    Invalid(String),
}

impl From<Denial> for RuleError {
    fn from(denial: Denial) -> Self {
        RuleError::Denied(denial)
    }
}

impl From<String> for RuleError {
    fn from(why: String) -> Self {
        RuleError::Invalid(why)
    }
}

/// Keys of a rule table that the rule uses.
const RULE_KEYS: [&str; 6] = [
    "enabled",
    "description",
    "allow",
    "deny",
    "passthrough",
    "inject",
];

/// A checked rule change, ready to apply.
struct RulePlan {
    /// The rule before the change, as it applies now: merged from all
    /// config files. `None` for a new rule.
    old: Option<NetworkRule>,
    /// The rule after the change, as it applies to the network: merged
    /// from all config files.
    rule: NetworkRule,
    /// Keys of the approved rule table in the project file that the rule
    /// does not use, as `(key, JSON value)`. The change keeps them.
    other: Vec<(String, String)>,
    /// The values to write into the project file.
    write: RuleWrite,
}

/// Check a rule change, ask the user, then write the rule into the project
/// config and apply it to new connections.
async fn change_rule(network: &Network, change: RuleChange) -> Result<(), RuleError> {
    let context = sandbox_context(network)?;
    let plan = plan_change(network, &context, &change)?;
    info!(
        "rule change: request for {:?}: enabled {}, allow [{}], deny [{}]",
        change.name,
        plan.rule.enabled.as_text(),
        plan.rule.allow.join(", "),
        plan.rule.deny.join(", ")
    );
    let prompt = PromptRequest::RuleChange {
        name: change.name.clone(),
        old: plan.old,
        rule: plan.rule,
        other: plan.other,
    };
    // A rule change has no running notice.
    ask_user(network, &context, prompt).await?;

    // Check again: the rules and the file can change while the user
    // decides. The write checks the file again.
    let plan = plan_change(network, &context, &change)?;
    let written = rule_file::write_rule(
        &context.project_dir,
        &plan.write,
        &mut context.approved_rules.borrow_mut(),
    );
    let (file, table) = written.map_err(|e| {
        warn!("rule change: {e:#}");
        format!("the rule could not be saved: {e:#}")
    })?;
    info!("rule change: {:?} saved to {file}", change.name);
    // The user approved this change. The next start must not ask again.
    // The table is the approved table with the change, not the text that
    // the file has now.
    if let Err(e) =
        crate::start::config_change::approve_rule(&context.sandbox_dir, &file, &change.name, table)
    {
        warn!("rule change: cannot update the config of the last run: {e:#}");
    }
    let mut rules = network.rules.borrow_mut();
    rules
        .upsert(&change.name, plan.rule)
        .map_err(|e| format!("the rule was saved, but it cannot apply now: {e:#}"))?;
    Ok(())
}

/// Check a rule change from the sandbox against `network.rule_editing`,
/// the current rules and the project file.
/// Returns:
///   The plan of the change. A deny with no prompt if the config does not
///   let the sandbox change the rule. Error text if the change is not
///   valid.
fn plan_change(
    network: &Network,
    context: &GrantContext,
    change: &RuleChange,
) -> Result<RulePlan, RuleError> {
    let name = &change.name;
    let editing = network.grants.rule_editing;
    if editing == RuleEditing::None {
        return Err(Denial::from(msg::RULE_EDITING_DISABLED).into());
    }
    let valid_name = !name.is_empty()
        && name.len() <= MAX_RULE_NAME
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !valid_name {
        return Err(format!(
            "rule name {name:?} is not valid: use a-z, 0-9, _ and -, \
             start with a letter or a digit, at most {MAX_RULE_NAME} characters"
        )
        .into());
    }
    let current = network.rules.borrow().rules().get(name).cloned();
    if editing == RuleEditing::OnDemand {
        let not_on_demand = |e: Option<RuleEnabled>| e.is_some_and(|e| e != RuleEnabled::OnDemand);
        if not_on_demand(current.as_ref().map(|r| r.enabled)) {
            return Err(Denial::from(msg::rule_name_taken(name)).into());
        }
        if not_on_demand(change.enabled) {
            return Err(Denial::from(msg::ONLY_ON_DEMAND_EDITABLE).into());
        }
    }
    if current.is_none() && change.allow.is_none() {
        return Err(format!("rule {name:?} is new. Give its `allow` patterns.").into());
    }

    // The project file can add only to the lists of the other config
    // files. Thus the patterns from the other files must stay. A rule in a
    // project file that is not the approved rule stops the change here,
    // with no prompt.
    let (file_allow, file_deny, file_table) =
        rule_file::file_rule(&context.project_dir, name, &context.approved_rules.borrow())
            .map_err(|e| format!("{e:#}"))?;
    let other = file_table
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(key, _)| !RULE_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.to_string()))
        .collect();
    let mut rule = current.clone().unwrap_or(NetworkRule {
        enabled: RuleEnabled::OnDemand,
        description: String::new(),
        allow: vec![],
        deny: vec![],
        passthrough: false,
        inject: vec![],
    });
    let mut write = RuleWrite {
        name: name.clone(),
        ..RuleWrite::default()
    };
    if let Some(given) = &change.allow {
        let (live, written) = merge_list(name, "allow", &rule.allow, &file_allow, given, |new| {
            Ok(parse_hosts(new)?.0)
        })?;
        rule.allow = live;
        write.allow = Some(written);
    }
    if let Some(given) = &change.deny {
        let (live, written) = merge_list(name, "deny", &rule.deny, &file_deny, given, check_deny)?;
        rule.deny = live;
        write.deny = Some(written);
    }
    if let Some(description) = &change.description {
        rule.description = crate::approval::text::cut_end(
            crate::approval::text::clean(description),
            crate::approval::text::MAX_DESCRIPTION,
        );
        write.description = Some(rule.description.clone());
    }
    if let Some(enabled) = change.enabled {
        rule.enabled = enabled;
        write.enabled = Some(enabled);
    }
    if current.is_none() {
        // A new rule gets all values in the file.
        write.enabled = Some(rule.enabled);
        write.description = Some(rule.description.clone());
        write.deny.get_or_insert_with(Vec::new);
    }
    // Compile the result now, so that a rule that cannot apply gives no
    // prompt.
    network
        .rules
        .borrow()
        .check(name, rule.clone())
        .with_context(|| format!("rule {name:?} cannot apply"))
        .map_err(|e| format!("{e:#}"))?;
    Ok(RulePlan {
        old: current,
        rule,
        other,
        write,
    })
}

/// Merge a new pattern list of the sandbox into a rule list.
/// Args:
///  - `name`, `key`: Rule name and list name, for the error text
///  - `current`: List of the rule as it applies now, from all config files
///  - `in_file`: List in the project file that the change goes to
///  - `given`: New list from the sandbox
///  - `check`: Checks the new patterns and gives their clean form
///
/// Returns:
///   The list that applies after the change and the list to write into
///   the project file. Error text if a new pattern is refused, the list is
///   too long, or the list drops a pattern of another config file.
fn merge_list(
    name: &str,
    key: &str,
    current: &[String],
    in_file: &[String],
    given: &[String],
    check: impl Fn(&[String]) -> Result<Vec<String>, String>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let given: Vec<String> = given.iter().map(|p| p.trim().to_string()).collect();
    // The patterns that come from other config files.
    let mut other: Vec<String> = current.to_vec();
    for p in in_file {
        if let Some(i) = other.iter().position(|o| o == p) {
            other.remove(i);
        }
    }
    let kept: Vec<&String> = other.iter().filter(|o| !given.contains(o)).collect();
    if !kept.is_empty() {
        let kept: Vec<&str> = kept.iter().map(|s| s.as_str()).collect();
        return Err(format!(
            "rule {name:?} gets {key} patterns from another config file: [{}]. \
             Keep them in `{key}`.",
            kept.join(", ")
        ));
    }
    let new: Vec<String> = given
        .iter()
        .filter(|p| !current.contains(p))
        .cloned()
        .collect();
    if new.len() > MAX_HOSTS {
        return Err(format!("too many new {key} patterns (maximum {MAX_HOSTS})"));
    }
    let new = if new.is_empty() { new } else { check(&new)? };
    let mut written: Vec<String> = Vec::new();
    for p in given.iter().filter(|p| current.contains(p)).chain(&new) {
        if !other.contains(p) && !written.contains(p) {
            written.push(p.clone());
        }
    }
    let live: Vec<String> = other.into_iter().chain(written.iter().cloned()).collect();
    if live.len() > MAX_RULE_PATTERNS {
        return Err(format!(
            "too many {key} patterns (maximum {MAX_RULE_PATTERNS})"
        ));
    }
    Ok((live, written))
}

/// Check new deny patterns of a rule change.
/// Returns:
///   The patterns with no duplicates, or the error text.
fn check_deny(patterns: &[String]) -> Result<Vec<String>, String> {
    let mut deny: Vec<String> = Vec::new();
    for raw in patterns {
        let clean = crate::approval::text::clean(raw);
        if raw.is_empty() || clean != *raw || raw.contains(' ') {
            return Err(format!("deny pattern {raw:?} is not valid"));
        }
        parse_pattern(raw).map_err(|e| e.to_string())?;
        if !deny.contains(raw) {
            deny.push(raw.clone());
        }
    }
    Ok(deny)
}

/// Live state of one grant: its access, its end time, its open
/// connections and its connection log.
struct GrantScope {
    access: GrantAccess,
    expires: Instant,
    ended: Cell<bool>,
    conns: RefCell<Vec<AbortHandle>>,
    /// Connect targets as `(host, port, blocked)`, one entry per target.
    log: RefCell<Vec<(String, u16, bool)>>,
    /// Notice on the host while the grant is active.
    running: RefCell<Option<Running>>,
    /// End of the grant: `None` while it is active, else true if the user
    /// aborted it.
    revoked: tokio::sync::watch::Sender<Option<bool>>,
}

impl GrantScope {
    /// Return true if the grant has not ended.
    fn is_live(&self) -> bool {
        !self.ended.get() && Instant::now() < self.expires
    }

    /// Add the relay task of a new connection.
    fn track(&self, conn: AbortHandle) {
        let mut conns = self.conns.borrow_mut();
        conns.retain(|c| !c.is_finished());
        conns.push(conn);
    }

    /// Add a connect target to the log. A target that the network blocked
    /// one time stays blocked in the log.
    fn record(&self, host: &str, port: u16, blocked: bool) {
        let mut log = self.log.borrow_mut();
        if let Some(entry) = log.iter_mut().find(|e| e.0 == host && e.1 == port) {
            entry.2 |= blocked;
        } else if log.len() < MAX_LOG {
            log.push((host.to_string(), port, blocked));
        }
    }

    /// End the grant, close all of its open connections and remove the
    /// notice on the host.
    /// Args:
    ///  - `why`: Reason for the log
    ///  - `aborted`: True if the user aborted the grant
    fn end(&self, why: &str, aborted: bool) {
        if self.ended.replace(true) {
            return;
        }
        let conns = std::mem::take(&mut *self.conns.borrow_mut());
        info!("grant: ended ({why})");
        for conn in conns {
            conn.abort();
        }
        drop(self.running.borrow_mut().take());
        self.revoked.send_replace(Some(aborted));
    }
}

/// Proxy of one grant. It applies the access of the grant in addition to
/// the normal policy. It refuses socket targets and new grant requests.
/// When the guest releases it (or the guest connection is lost), the grant
/// ends.
struct ScopedProxy {
    network: Rc<Network>,
    scope: Rc<GrantScope>,
    timer: AbortHandle,
}

impl ScopedProxy {
    /// Make the proxy and start the expiry timer.
    /// Args:
    ///  - `network`: Network of the sandbox
    ///  - `access`: Extra access of the grant
    ///  - `duration`: Duration of the grant
    ///  - `running`: Notice on the host. An abort from it ends the grant.
    fn start(
        network: Rc<Network>,
        access: GrantAccess,
        duration: Duration,
        mut running: Option<Running>,
    ) -> Self {
        let abort = running.as_mut().and_then(Running::take_abort);
        let scope = Rc::new(GrantScope {
            access,
            expires: Instant::now() + duration,
            ended: Cell::new(false),
            conns: RefCell::default(),
            log: RefCell::default(),
            running: RefCell::new(running),
            revoked: tokio::sync::watch::Sender::new(None),
        });
        // The timer holds a weak reference. The proxy owns the scope.
        let weak = Rc::downgrade(&scope);
        let expires = scope.expires;
        let timer = tokio::task::spawn_local(async move {
            // An error means that the notice went away with no abort. Then
            // only the expiry ends the grant.
            let abort = async move {
                if let Some(abort) = abort
                    && abort.await.is_ok()
                {
                    return;
                }
                std::future::pending::<()>().await;
            };
            let aborted = tokio::select! {
                () = tokio::time::sleep_until(expires) => false,
                () = abort => true,
            };
            if let Some(scope) = weak.upgrade() {
                if aborted {
                    scope.end("aborted by the user", true);
                } else {
                    scope.end("expired", false);
                }
            }
        })
        .abort_handle();
        Self {
            network,
            scope,
            timer,
        }
    }
}

impl Drop for ScopedProxy {
    fn drop(&mut self) {
        self.timer.abort();
        self.scope.end("released", false);
    }
}

impl network_proxy::Server for ScopedProxy {
    async fn connect(
        self: Rc<Self>,
        params: network_proxy::ConnectParams,
        mut results: network_proxy::ConnectResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let target = params.get_target()?;
        let client_sink = params.get_client()?;
        let tcp = match target.which()? {
            connect_target::Tcp(tcp) => tcp?,
            connect_target::Socket(_) => {
                results
                    .get()
                    .init_result()
                    .set_denied("a grant does not allow sockets");
                return Ok(());
            }
        };
        let host = tcp.get_host()?.to_str()?;
        let port = tcp.get_port();
        if !self.scope.is_live() {
            debug!("grant: denied {host}:{port} (grant ended)");
            results.get().init_result().set_denied("the grant ended");
            return Ok(());
        }
        let (sink, conn, allowed) =
            self.network
                .open_tcp(host, port, client_sink, Some(&self.scope.access));
        self.scope.record(host, port, !allowed);
        self.scope.track(conn);
        results.get().init_result().set_server(sink);
        Ok(())
    }

    async fn request_grant(
        self: Rc<Self>,
        _params: network_proxy::RequestGrantParams,
        mut results: network_proxy::RequestGrantResults,
    ) -> Result<(), capnp::Error> {
        let denial = Denial::from("a grant cannot ask for another grant");
        set_denied(results.get().init_result().init_denied(), &denial);
        Ok(())
    }

    async fn wait_revoked(
        self: Rc<Self>,
        _params: network_proxy::WaitRevokedParams,
        mut results: network_proxy::WaitRevokedResults,
    ) -> Result<(), capnp::Error> {
        let mut revoked = self.scope.revoked.subscribe();
        // A pending call must not keep the proxy alive. Then a release by
        // the guest still ends the grant.
        drop(self);
        let aborted = match revoked.wait_for(Option::is_some).await {
            Ok(end) => end.unwrap_or(false),
            Err(_) => false,
        };
        results.get().set_aborted(aborted);
        Ok(())
    }

    async fn grant_log(
        self: Rc<Self>,
        _params: network_proxy::GrantLogParams,
        mut results: network_proxy::GrantLogResults,
    ) -> Result<(), capnp::Error> {
        let log = self.scope.log.borrow();
        let mut list = results.get().init_entries(log.len() as u32);
        for (i, (host, port, blocked)) in log.iter().enumerate() {
            let mut entry = list.reborrow().get(i as u32);
            entry.set_host(host);
            entry.set_port(*port);
            entry.set_blocked(*blocked);
        }
        Ok(())
    }
}
