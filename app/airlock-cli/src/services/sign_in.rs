//! Loopback sign-ins: the browser grant of a network service.
//!
//! The agent signs in inside the sandbox; its sign-in page opens in the
//! host's browser through the browser bridge ([`crate::rpc::browser`]),
//! and the agent listens for the OAuth callback on a loopback port of the
//! guest. [`LoopbackSignIn`] is the service's
//! [`crate::rpc::browser::BrowserGrant`]: it decides which pages the guest
//! may open and forwards their callback port from host loopback into the
//! guest ([`super::callback`]).
//!
//! The VM is untrusted, so a page opens only when it is one of the
//! service's [`SignInPage`]s and its OAuth parameters match the page:
//!
//! - one non-empty `client_id` (any: an agent can have several OAuth
//!   clients, and the code exchange stores the one it used),
//!   `response_type=code`, one `code_challenge` with
//!   `code_challenge_method=S256`, and one `scope` with at least one
//!   scope, each a well-formed scope token (RFC 6749, section 3.3) and
//!   named once (any scope: the provider decides what it grants); no
//!   `response_mode`, no `prompt=none`;
//! - exactly one loopback `redirect_uri` on a port and path of the page
//!   (no arbitrary host ports).
//!
//! The browser checks the URL itself (`https`, no user name, port or
//! fragment, length, characters) before it asks the grants.
//!
//! The `code_challenge` of every page that opens is kept
//! ([`PendingCodes::open_page`]): Claude's manual sign-in exchange, whose
//! real code the user pastes, works only with the verifier of a page
//! opened here.
//!
//! The callback port is bound exclusively, so the guest never receives
//! traffic meant for a host program. One forward runs at a time: a
//! sign-in that names a new callback port replaces the previous forward
//! (Claude Code listens on a new ephemeral port per sign-in); a new
//! sign-in on the same port opens the forward again. Forwards need the
//! booted VM: they run between [`LoopbackSignIn::attach`] and
//! [`LoopbackSignIn::detach`]. A forward frees its port once it closes
//! ([`super::callback`]: no code in time, or the follow-ups after the
//! code are over). Refusals name the host and path only,
//! never the query: it carries OAuth state.

use std::cell::RefCell;
use std::collections::HashSet;
use std::ops::RangeInclusive;

use tokio::task::JoinSet;
use url::Url;

use super::ServiceId;
use super::auth_codes::PendingCodes;
use super::callback::{self, Callback, CallbackForward};
use crate::network::reverse_forward;
use crate::rpc::browser::{BrowserGrant, GrantAnswer};
use crate::rpc::guest_network::GuestNetwork;

/// A sign-in page the guest may open, with the callback it may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInPage {
    /// Exact host of the authorize page (https, default port).
    pub host: &'static str,
    /// Exact path of the authorize page.
    pub path: &'static str,
    /// Allowed ports of the `redirect_uri`: the loopback ports the sign-in
    /// tool may listen on for its callback.
    pub callback_ports: &'static [RangeInclusive<u16>],
    /// Required path of the `redirect_uri`.
    pub callback_path: &'static str,
    /// Hosts of the service's pages the callback may redirect the browser
    /// to (`https`): the authorize hosts and their success pages.
    pub pages: &'static [&'static str],
}

impl SignInPage {
    fn is_page_of(&self, url: &Url) -> bool {
        url.host_str() == Some(self.host) && url.path() == self.path
    }
}

/// The sign-ins of one service: its pages, and the callback forward of
/// the current sign-in.
pub struct LoopbackSignIn {
    service: ServiceId,
    pages: Vec<SignInPage>,
    /// Where the callback forwards keep the codes they swapped, and the
    /// grant keeps the challenges of the pages it opened.
    codes: PendingCodes,
    /// The guest from [`Self::attach`] until [`Self::detach`]. Runtime
    /// state: the grant exists before the VM boots, the guest only after.
    guest: RefCell<Option<GuestNetwork>>,
    /// The current callback forward. Runtime state: each sign-in may
    /// replace it.
    forward: RefCell<Option<Forward>>,
}

/// A callback port forwarded into the guest.
struct Forward {
    port: u16,
    /// The accept loops; dropping the set aborts them.
    tasks: JoinSet<()>,
    state: CallbackForward,
}

impl LoopbackSignIn {
    /// The sign-ins of `service` on `pages`; their callback forwards swap
    /// codes into `codes`.
    pub fn new(service: ServiceId, pages: Vec<SignInPage>, codes: PendingCodes) -> Self {
        Self {
            service,
            pages,
            codes,
            guest: RefCell::new(None),
            forward: RefCell::new(None),
        }
    }

    /// Forward callbacks into `guest` from now on.
    pub fn attach(&self, guest: &GuestNetwork) {
        self.guest.replace(Some(guest.clone()));
    }

    /// Stop forwarding: the callback port is free once this returns, and
    /// later sign-ins are refused.
    pub async fn detach(&self) {
        self.guest.replace(None);
        let forward = self.forward.take();
        if let Some(Forward { mut tasks, .. }) = forward {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
    }

    /// Forward `port` for a sign-in of `callback`. A new port, or the port
    /// of a forward that has closed, replaces the current forward only
    /// once its bind succeeded. Synchronous from the check to the bind, so
    /// concurrent sign-ins cannot race past it.
    fn forward(&self, port: u16, callback: Callback) -> Result<(), String> {
        let guest = self.guest.borrow();
        let Some(guest) = guest.as_ref() else {
            return Err("the sandbox is not ready for a sign-in".into());
        };
        let mut current = self.forward.borrow_mut();
        if let Some(forward) = current.as_ref().filter(|f| f.port == port)
            && forward.state.reopen(callback)
        {
            // A new sign-in on the same port.
            return Ok(());
        }
        match reverse_forward::bind_exclusive(port, port) {
            Ok(bound) => {
                let mut tasks = JoinSet::new();
                let state = callback::serve(bound, guest, &mut tasks, &self.codes, callback);
                // The old forward's accept loops end with its set.
                *current = Some(Forward { port, tasks, state });
                tracing::debug!("sign-in: forwarding callback port {port}");
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Err(format!(
                "port {port} is in use on this computer, so the browser cannot \
                 reach the sign-in in the sandbox"
            )),
            Err(e) => Err(format!(
                "could not listen on port {port} for the sign-in: {e}"
            )),
        }
    }
}

impl BrowserGrant for LoopbackSignIn {
    fn allow(&self, url: &Url) -> GrantAnswer {
        let Some(page) = self.pages.iter().find(|p| p.is_page_of(url)) else {
            return GrantAnswer::NotMine;
        };
        let place = format!("{}{}", page.host, page.path);
        let checked = match check_params(url, page) {
            Ok(checked) => checked,
            Err(e) => return GrantAnswer::Refuse(format!("refused to open a page: {place}: {e}")),
        };
        let callback = Callback {
            service: self.service,
            pages: page.pages,
        };
        match self.forward(checked.port, callback) {
            Ok(()) => {
                self.codes.open_page(&checked.challenge, self.service);
                GrantAnswer::Allow
            }
            Err(e) => GrantAnswer::Refuse(e),
        }
    }
}

/// What [`check_params`] found in an authorize URL.
#[derive(Debug)]
struct Checked {
    /// The port of the loopback `redirect_uri`.
    port: u16,
    /// The PKCE `code_challenge`.
    challenge: String,
}

/// The OAuth parameters of an authorize URL against its page. The error
/// never contains the query.
fn check_params(url: &Url, page: &SignInPage) -> Result<Checked, String> {
    let one = |name: &str| -> Result<String, String> {
        let values: Vec<String> = url
            .query_pairs()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
            .collect();
        match <[String; 1]>::try_from(values) {
            Ok([value]) => Ok(value),
            Err(values) => Err(format!("expected one {name}, found {}", values.len())),
        }
    };
    if one("client_id")?.is_empty() {
        return Err("the client_id is empty".into());
    }
    if one("response_type")? != "code" {
        return Err("the response_type is not code".into());
    }
    if one("code_challenge_method")? != "S256" {
        return Err("the code_challenge_method is not S256".into());
    }
    let challenge = one("code_challenge")?;
    if challenge.is_empty() {
        return Err("the code_challenge is empty".into());
    }
    let scope = one("scope")?;
    let scopes: Vec<&str> = scope.split(' ').collect();
    if scopes.iter().any(|s| !is_scope_token(s)) {
        return Err("the scope is malformed".into());
    }
    if scopes.iter().collect::<HashSet<_>>().len() != scopes.len() {
        return Err("the scope names a scope twice".into());
    }
    for (k, v) in url.query_pairs() {
        if k == "response_mode" {
            return Err("the URL sets a response_mode".into());
        }
        if k == "prompt" && v.split_whitespace().any(|p| p == "none") {
            return Err("the URL asks for prompt=none".into());
        }
    }
    let port = check_redirect(&one("redirect_uri")?, page)?;
    Ok(Checked { port, challenge })
}

/// A scope token of RFC 6749, section 3.3: one or more of `%x21 /
/// %x23-5B / %x5D-7E` (visible ASCII without `"` and `\`).
fn is_scope_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|c| matches!(c, 0x21 | 0x23..=0x5B | 0x5D..=0x7E))
}

/// The redirect must be `http://localhost|127.0.0.1:<port><callback_path>`
/// with the port in the page's set. Returns the port.
fn check_redirect(raw: &str, page: &SignInPage) -> Result<u16, String> {
    let url = Url::parse(raw).map_err(|_| "the redirect_uri is not a URL".to_string())?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    if url.scheme() != "http" || !loopback {
        return Err("the redirect_uri is not an http loopback address".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("the redirect_uri carries a user name".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("the redirect_uri has a query".into());
    }
    if url.path() != page.callback_path {
        return Err(format!(
            "the redirect_uri path is not {}",
            page.callback_path
        ));
    }
    let port = url
        .port()
        .ok_or_else(|| "the redirect_uri names no port".to_string())?;
    if !page.callback_ports.iter().any(|r| r.contains(&port)) {
        return Err(format!("callback port {port} is not allowed"));
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use airlock_common::BROWSER_URL_MAX;

    use super::*;
    use crate::rpc::browser::checked_url;
    use crate::test_cfg::block_on_local;
    use crate::test_cfg::services::{free_claude_callback_port, idle_guest};

    const CLAUDE_URL: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A35527%2Fcallback&scope=user%3Ainference&code_challenge=Zm9vYmFyYmF6cXV4&code_challenge_method=S256&state=c3RhdGVzdGF0ZQ";
    const CODEX_URL: &str = "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&scope=openid%20profile%20email%20offline_access%20api.connectors.read%20api.connectors.invoke&code_challenge=Zm9vYmFy&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state=c3RhdGU&originator=codex_cli_rs";

    fn claude() -> Vec<SignInPage> {
        crate::services::anthropic::sign_in_pages()
    }

    fn codex() -> Vec<SignInPage> {
        crate::services::openai::sign_in_pages()
    }

    fn check_url(raw: &str, pages: &[SignInPage]) -> Result<u16, String> {
        let url = checked_url(raw)?;
        let page = pages
            .iter()
            .find(|p| p.is_page_of(&url))
            .ok_or_else(|| "not a sign-in page airlock knows".to_string())?;
        check_params(&url, page).map(|c| c.port)
    }

    #[test]
    fn agent_sign_in_urls_pass_with_callback_port_of_their_own_page() {
        let mut both = claude();
        both.extend(codex());
        assert_eq!(check_url(CLAUDE_URL, &both).unwrap(), 35527);
        assert_eq!(checked_url(CLAUDE_URL).unwrap().as_str(), CLAUDE_URL);
        let console = CLAUDE_URL.replace("claude.com/cai/oauth", "platform.claude.com/oauth");
        assert!(check_url(&console, &both).is_ok());
        assert_eq!(check_url(CODEX_URL, &both).unwrap(), 1455);
        let alt = CODEX_URL.replace("1455", "1457");
        assert_eq!(check_url(&alt, &both).unwrap(), 1457);
        let outside = CODEX_URL.replace("1455", "1456");
        let err = check_url(&outside, &both).unwrap_err();
        assert!(err.ends_with("callback port 1456 is not allowed"), "{err}");
        let v4 = CLAUDE_URL.replace("localhost", "127.0.0.1");
        assert!(check_url(&v4, &both).is_ok());
        let named = CODEX_URL.replace("127.0.0.1", "localhost");
        assert!(check_url(&named, &both).is_ok());
        let crossed = CLAUDE_URL.replace("35527%2Fcallback", "1455%2Fauth%2Fcallback");
        assert!(check_url(&crossed, &both).is_err());
        let crossed = CODEX_URL.replace("1455%2Fauth%2Fcallback", "35527%2Fcallback");
        assert!(check_url(&crossed, &both).is_err());
        assert!(check_url(CLAUDE_URL, &codex()).is_err());
        assert!(check_url(CODEX_URL, &claude()).is_err());
    }

    #[test]
    fn url_that_breaks_oauth_parameter_rule_is_refused_without_naming_query() {
        let p = claude();
        let bad = [
            CLAUDE_URL.replace("claude.com/cai", "claude.com.evil.io/cai"),
            CLAUDE_URL.replace("/cai/oauth/authorize", "/cai/oauth/authorize2"),
            CLAUDE_URL.replace("https://claude.com", "https://evil.com"),
            CLAUDE_URL.replace("https:", "http:"),
            "file:///etc/passwd".into(),
            "javascript:alert(1)".into(),
            CLAUDE_URL.replace("https://claude.com", "https://u:p@claude.com"),
            CLAUDE_URL.replace("https://claude.com", "https://claude.com:8443"),
            format!("{CLAUDE_URL}#x"),
            format!("{CLAUDE_URL}&redirect_uri=http%3A%2F%2Flocalhost%3A35528%2Fcallback"),
            CLAUDE_URL.replace("redirect_uri=", "redirect="),
            CLAUDE_URL.replace("localhost%3A35527", "evil.com%3A35527"),
            CLAUDE_URL.replace("localhost%3A35527", "0.0.0.0%3A35527"),
            CLAUDE_URL.replace("http%3A%2F%2Flocalhost", "https%3A%2F%2Flocalhost"),
            CLAUDE_URL.replace("%3A35527", ""),
            CLAUDE_URL.replace("35527", "22"),
            CLAUDE_URL.replace("35527", "61000"),
            CLAUDE_URL.replace("%2Fcallback", "%2Fcallback2"),
            CLAUDE_URL.replace("%2Fcallback", "%2Fcallback%3Fx%3D1"),
            format!("{CLAUDE_URL}&x=$(id)"),
            format!("{CLAUDE_URL}&x=a;b"),
            format!("{CLAUDE_URL}&x=`id`"),
            format!("{CLAUDE_URL}&pad={}", "a".repeat(BROWSER_URL_MAX)),
            CLAUDE_URL.replace(
                "client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                "client_id=",
            ),
            format!("{CLAUDE_URL}&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
            CLAUDE_URL.replace("client_id=", "client="),
            CLAUDE_URL.replace("response_type=code", "response_type=token"),
            format!("{CLAUDE_URL}&response_type=code"),
            CLAUDE_URL.replace("code_challenge_method=S256", "code_challenge_method=plain"),
            CLAUDE_URL.replace("&code_challenge_method=S256", ""),
            CLAUDE_URL.replace("&code_challenge=Zm9vYmFyYmF6cXV4", ""),
            CLAUDE_URL.replace("code_challenge=Zm9vYmFyYmF6cXV4", "code_challenge="),
            format!("{CLAUDE_URL}&code_challenge=Zm9vYmFyYmF6cXV4"),
            CLAUDE_URL.replace("scope=user%3Ainference", "scope=user%3Ainference%20a%22b"),
            CLAUDE_URL.replace("scope=user%3Ainference", "scope=a%5Cb"),
            CLAUDE_URL.replace("scope=user%3Ainference", "scope=a%20%20b"),
            CLAUDE_URL.replace("scope=user%3Ainference", "scope=a%09b"),
            CLAUDE_URL.replace("scope=user%3Ainference", "scope=%C3%A9"),
            CLAUDE_URL.replace("scope=user%3Ainference", "scope=%20a"),
            CLAUDE_URL.replace("scope=user%3Ainference", "scope="),
            CLAUDE_URL.replace("&scope=user%3Ainference", ""),
            format!("{CLAUDE_URL}&scope=user%3Aprofile"),
            CLAUDE_URL.replace(
                "scope=user%3Ainference",
                "scope=user%3Ainference%20user%3Ainference",
            ),
            CLAUDE_URL.replace(
                "scope=user%3Ainference",
                "scope=user%3Ainference+user%3Aprofile+user%3Ainference",
            ),
            format!("{CLAUDE_URL}&response_mode=form_post"),
            format!("{CLAUDE_URL}&prompt=none"),
            format!("{CLAUDE_URL}&prompt=login%20none"),
        ];
        for url in &bad {
            let msg = check_url(url, &p).expect_err(url);
            assert!(
                !msg.contains("Zm9vYmFyYmF6cXV4") && !msg.contains("c3RhdGVzdGF0ZQ"),
                "{url}: {msg}"
            );
        }
        assert!(check_url(&format!("{CLAUDE_URL}&prompt=login"), &p).is_ok());
        let all = CLAUDE_URL.replace(
            "scope=user%3Ainference",
            "scope=org%3Acreate_api_key%20user%3Aprofile%20user%3Ainference%20user%3Asessions%3Aclaude_code%20user%3Amcp_servers%20user%3Afile_upload%20user%3Aplugins",
        );
        assert!(check_url(&all, &p).is_ok());
        let new = CLAUDE_URL.replace(
            "scope=user%3Ainference",
            "scope=user%3Ainference+new%3Ascope%21",
        );
        assert!(check_url(&new, &p).is_ok());
    }

    fn claude_sign_in() -> LoopbackSignIn {
        let sign_in = LoopbackSignIn::new(ServiceId::Anthropic, claude(), PendingCodes::default());
        sign_in.attach(&idle_guest());
        sign_in
    }

    fn allow(sign_in: &LoopbackSignIn, port: u16) -> GrantAnswer {
        let raw = CLAUDE_URL.replace("35527", &port.to_string());
        sign_in.allow(&Url::parse(&raw).unwrap())
    }

    #[test]
    fn sign_in_on_new_port_replaces_forward_and_detach_frees_port() {
        block_on_local(async {
            let sign_in = claude_sign_in();
            let first = free_claude_callback_port();
            assert_eq!(allow(&sign_in, first), GrantAnswer::Allow);
            assert!(reverse_forward::bind_exclusive(first, first).is_err());
            assert_eq!(allow(&sign_in, first), GrantAnswer::Allow);
            let second = free_claude_callback_port();
            assert_eq!(allow(&sign_in, second), GrantAnswer::Allow);
            assert!(reverse_forward::bind_exclusive(second, second).is_err());
            let mut freed = false;
            for _ in 0..50 {
                if let Ok(l) = reverse_forward::bind_exclusive(first, first) {
                    drop(l);
                    freed = true;
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(freed, "the replaced forward still holds port {first}");
            sign_in.detach().await;
            drop(reverse_forward::bind_exclusive(second, second).unwrap());
        });
    }

    #[test]
    fn sign_in_on_busy_port_is_refused_and_keeps_forward() {
        block_on_local(async {
            let sign_in = claude_sign_in();
            let held = free_claude_callback_port();
            assert_eq!(allow(&sign_in, held), GrantAnswer::Allow);
            let port = free_claude_callback_port();
            let _busy = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
            let answer = allow(&sign_in, port);
            assert!(
                matches!(&answer, GrantAnswer::Refuse(r) if r.contains("in use")),
                "{answer:?}"
            );
            assert!(reverse_forward::bind_exclusive(held, held).is_err());
            sign_in.detach().await;
        });
    }
}
