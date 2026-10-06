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
//! - the page's `client_id`, `response_type=code`,
//!   `code_challenge_method=S256`, and scopes of the page's set (each
//!   exactly once); no `response_mode`, no `prompt=none`;
//! - exactly one loopback `redirect_uri` on a port and path of the page
//!   (no arbitrary host ports).
//!
//! The browser checks the URL itself (`https`, no user name, port or
//! fragment, length, characters) before it asks the grants.
//!
//! The callback port is bound exclusively, so the guest never receives
//! traffic meant for a host program. One forward runs at a time: a
//! sign-in that names a new callback port replaces the previous forward
//! (Claude Code listens on a new ephemeral port per sign-in); a new
//! sign-in on the same port opens the forward again. Forwards need the
//! booted VM: they run between [`LoopbackSignIn::attach`] and
//! [`LoopbackSignIn::detach`]. Refusals name the host and path only,
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
    /// Required `client_id`.
    pub client_id: &'static str,
    /// The scopes the `scope` parameter may name.
    pub scopes: &'static [&'static str],
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
    /// Where the callback forwards keep the codes they swapped.
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

    /// Forward `port` for a sign-in of `callback`. A new port replaces the
    /// current forward only once its bind succeeded. Synchronous from the
    /// check to the bind, so concurrent sign-ins cannot race past it.
    fn forward(&self, port: u16, callback: Callback) -> Result<(), String> {
        let guest = self.guest.borrow();
        let Some(guest) = guest.as_ref() else {
            return Err("the sandbox is not ready for a sign-in".into());
        };
        let mut current = self.forward.borrow_mut();
        if let Some(forward) = current.as_ref().filter(|f| f.port == port) {
            // A new sign-in on the same port.
            forward.state.reopen(callback);
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
        let port = match check_params(url, page) {
            Ok(port) => port,
            Err(e) => return GrantAnswer::Refuse(format!("refused to open a page: {place}: {e}")),
        };
        let callback = Callback {
            service: self.service,
            pages: page.pages,
        };
        match self.forward(port, callback) {
            Ok(()) => GrantAnswer::Allow,
            Err(e) => GrantAnswer::Refuse(e),
        }
    }
}

/// The OAuth parameters of an authorize URL against its page. Returns
/// the callback port. The error never contains the query.
fn check_params(url: &Url, page: &SignInPage) -> Result<u16, String> {
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
    if one("client_id")? != page.client_id {
        return Err("the client_id is not the agent's".into());
    }
    if one("response_type")? != "code" {
        return Err("the response_type is not code".into());
    }
    if one("code_challenge_method")? != "S256" {
        return Err("the code_challenge_method is not S256".into());
    }
    let scope = one("scope")?;
    let scopes: Vec<&str> = scope.split_whitespace().collect();
    if scopes.is_empty() || scopes.iter().any(|s| !page.scopes.contains(s)) {
        return Err("the scope names a scope airlock does not know".into());
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
    check_redirect(&one("redirect_uri")?, page)
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
    use airlock_common::supervisor_capnp::supervisor;

    use super::*;
    use crate::rpc::browser::checked_url;
    use crate::test_support::block_on_local;

    /// `auth-claude.md`: the URL claude passes to `$BROWSER`.
    const CLAUDE_URL: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A35527%2Fcallback&scope=user%3Ainference&code_challenge=Zm9vYmFyYmF6cXV4&code_challenge_method=S256&state=c3RhdGVzdGF0ZQ";
    /// `auth-codex.md`: the shape of the URL `codex login` opens.
    const CODEX_URL: &str = "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&scope=openid%20profile%20email%20offline_access%20api.connectors.read%20api.connectors.invoke&code_challenge=Zm9vYmFy&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state=c3RhdGU&originator=codex_cli_rs";

    fn claude() -> Vec<SignInPage> {
        crate::services::anthropic::sign_in_pages()
    }

    fn codex() -> Vec<SignInPage> {
        crate::services::openai::sign_in_pages()
    }

    /// Both services' pages.
    fn both() -> Vec<SignInPage> {
        let mut pages = claude();
        pages.extend(codex());
        pages
    }

    /// The browser's URL check and the page's parameter check, without a
    /// forward: the callback port, or the refusal.
    fn check_url(raw: &str, pages: &[SignInPage]) -> Result<u16, String> {
        let url = checked_url(raw)?;
        let page = pages
            .iter()
            .find(|p| p.is_page_of(&url))
            .ok_or_else(|| "not a sign-in page airlock knows".to_string())?;
        check_params(&url, page)
    }

    #[test]
    fn fixture_urls_pass() {
        assert_eq!(check_url(CLAUDE_URL, &claude()).unwrap(), 35527);
        assert_eq!(checked_url(CLAUDE_URL).unwrap().as_str(), CLAUDE_URL);
        let console = CLAUDE_URL.replace("claude.com/cai/oauth", "platform.claude.com/oauth");
        assert!(check_url(&console, &claude()).is_ok());
        assert_eq!(check_url(CODEX_URL, &codex()).unwrap(), 1455);
        let alt = CODEX_URL.replace("1455", "1457");
        assert_eq!(check_url(&alt, &codex()).unwrap(), 1457);
        let outside = CODEX_URL.replace("1455", "1456");
        let err = check_url(&outside, &codex()).unwrap_err();
        assert!(err.ends_with("callback port 1456 is not allowed"), "{err}");
        let v4 = CLAUDE_URL.replace("localhost", "127.0.0.1");
        assert!(check_url(&v4, &claude()).is_ok());
        let named = CODEX_URL.replace("127.0.0.1", "localhost");
        assert!(check_url(&named, &codex()).is_ok());
    }

    #[test]
    fn rejections() {
        let p = claude();
        let bad = [
            // wrong host or path
            CLAUDE_URL.replace("claude.com/cai", "claude.com.evil.io/cai"),
            CLAUDE_URL.replace("/cai/oauth/authorize", "/cai/oauth/authorize2"),
            CLAUDE_URL.replace("https://claude.com", "https://evil.com"),
            // schemes
            CLAUDE_URL.replace("https:", "http:"),
            "file:///etc/passwd".into(),
            "javascript:alert(1)".into(),
            // userinfo, explicit port, fragment
            CLAUDE_URL.replace("https://claude.com", "https://u:p@claude.com"),
            CLAUDE_URL.replace("https://claude.com", "https://claude.com:8443"),
            format!("{CLAUDE_URL}#x"),
            // two redirects, none
            format!("{CLAUDE_URL}&redirect_uri=http%3A%2F%2Flocalhost%3A35528%2Fcallback"),
            CLAUDE_URL.replace("redirect_uri=", "redirect=").clone(),
            // non-loopback redirect, https redirect, no port
            CLAUDE_URL.replace("localhost%3A35527", "evil.com%3A35527"),
            CLAUDE_URL.replace("localhost%3A35527", "0.0.0.0%3A35527"),
            CLAUDE_URL.replace("http%3A%2F%2Flocalhost", "https%3A%2F%2Flocalhost"),
            CLAUDE_URL.replace("%3A35527", ""),
            // port outside the set, wrong callback path, redirect query
            CLAUDE_URL.replace("35527", "22"),
            CLAUDE_URL.replace("35527", "61000"),
            CLAUDE_URL.replace("%2Fcallback", "%2Fcallback2"),
            CLAUDE_URL.replace("%2Fcallback", "%2Fcallback%3Fx%3D1"),
            // metacharacters
            format!("{CLAUDE_URL}&x=$(id)"),
            format!("{CLAUDE_URL}&x=a;b"),
            format!("{CLAUDE_URL}&x=`id`"),
            // oversize
            format!("{CLAUDE_URL}&pad={}", "a".repeat(BROWSER_URL_MAX)),
            // another client, two clients, no client
            CLAUDE_URL.replace("client_id=9d1c", "client_id=0d1c"),
            format!("{CLAUDE_URL}&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
            CLAUDE_URL.replace("client_id=", "client=").clone(),
            // implicit flow, plain PKCE, no PKCE method
            CLAUDE_URL.replace("response_type=code", "response_type=token"),
            format!("{CLAUDE_URL}&response_type=code"),
            CLAUDE_URL.replace("code_challenge_method=S256", "code_challenge_method=plain"),
            CLAUDE_URL.replace("&code_challenge_method=S256", ""),
            // scopes: unknown, empty, missing, twice
            CLAUDE_URL.replace("scope=user%3Ainference", "scope=user%3Ainference%20admin"),
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
            // a response mode, a silent sign-in
            format!("{CLAUDE_URL}&response_mode=form_post"),
            format!("{CLAUDE_URL}&prompt=none"),
            format!("{CLAUDE_URL}&prompt=login%20none"),
        ];
        for url in &bad {
            let got = check_url(url, &p);
            assert!(got.is_err(), "accepted {url}");
            // The message never repeats the query's values.
            let msg = got.unwrap_err();
            assert!(
                !msg.contains("Zm9vYmFyYmF6cXV4") && !msg.contains("c3RhdGVzdGF0ZQ"),
                "{url}: {msg}"
            );
        }
        // A prompt that is not silent, and all of Claude's login scopes.
        assert!(check_url(&format!("{CLAUDE_URL}&prompt=login"), &p).is_ok());
        let all = CLAUDE_URL.replace(
            "scope=user%3Ainference",
            "scope=org%3Acreate_api_key%20user%3Aprofile%20user%3Ainference%20user%3Asessions%3Aclaude_code%20user%3Amcp_servers%20user%3Afile_upload%20user%3Aplugins",
        );
        assert!(check_url(&all, &p).is_ok());
        // The codex pages refuse the claude page and vice versa.
        assert!(check_url(CLAUDE_URL, &codex()).is_err());
        assert!(check_url(CODEX_URL, &p).is_err());
    }

    /// With several pages, each page is checked against its own callback
    /// rule: a codex callback on the claude page is refused, and the
    /// other way round.
    #[test]
    fn several_rules_keep_their_callbacks_apart() {
        let p = both();
        assert_eq!(check_url(CLAUDE_URL, &p).unwrap(), 35527);
        assert_eq!(check_url(CODEX_URL, &p).unwrap(), 1455);
        let crossed = CLAUDE_URL.replace("35527%2Fcallback", "1455%2Fauth%2Fcallback");
        assert!(check_url(&crossed, &p).is_err());
        let crossed = CODEX_URL.replace("1455%2Fauth%2Fcallback", "35527%2Fcallback");
        assert!(check_url(&crossed, &p).is_err());
    }

    /// A supervisor that implements nothing: the forward's accept loops
    /// only call it for a connection.
    struct NoSupervisor;
    impl supervisor::Server for NoSupervisor {}

    /// Claude's sign-ins, attached to a guest that is never reached.
    fn claude_sign_in() -> LoopbackSignIn {
        let sign_in = LoopbackSignIn::new(ServiceId::Anthropic, claude(), PendingCodes::default());
        sign_in.attach(&GuestNetwork::new(capnp_rpc::new_client(NoSupervisor)));
        sign_in
    }

    fn allow(sign_in: &LoopbackSignIn, raw: &str) -> GrantAnswer {
        sign_in.allow(&Url::parse(raw).unwrap())
    }

    /// A free port inside the claude range.
    fn free_port() -> u16 {
        loop {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            if (32768..=60999).contains(&port) {
                return port;
            }
        }
    }

    fn claude_url(port: u16) -> String {
        CLAUDE_URL.replace("35527", &port.to_string())
    }

    /// A sign-in with a new callback port replaces the forward of the
    /// previous one; the same port keeps it. Detaching frees the port.
    #[test]
    fn a_new_port_replaces_the_forward_and_detach_frees_it() {
        block_on_local(async {
            let sign_in = claude_sign_in();
            let first = free_port();
            assert_eq!(allow(&sign_in, &claude_url(first)), GrantAnswer::Allow);
            // Held: another exclusive bind fails.
            assert!(reverse_forward::bind_exclusive(first, first).is_err());
            assert_eq!(allow(&sign_in, &claude_url(first)), GrantAnswer::Allow);
            let second = free_port();
            assert_eq!(allow(&sign_in, &claude_url(second)), GrantAnswer::Allow);
            assert!(reverse_forward::bind_exclusive(second, second).is_err());
            // The aborted accept loop lets go of the first port.
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

    /// A busy port is refused and keeps the current forward.
    #[test]
    fn a_busy_port_is_not_opened_and_keeps_the_forward() {
        block_on_local(async {
            let sign_in = claude_sign_in();
            let held = free_port();
            assert_eq!(allow(&sign_in, &claude_url(held)), GrantAnswer::Allow);
            let port = free_port();
            let _busy = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
            let answer = allow(&sign_in, &claude_url(port));
            assert!(
                matches!(&answer, GrantAnswer::Refuse(r) if r.contains("in use")),
                "{answer:?}"
            );
            assert!(reverse_forward::bind_exclusive(held, held).is_err());
            sign_in.detach().await;
        });
    }
}
