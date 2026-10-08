//! Host-side browser bridge.
//!
//! Lets the guest ask the host to open a page, for example a sign-in page,
//! in the user's browser. The VM is untrusted, so the host checks each
//! request before it opens a page. The caller decides which pages can open,
//! so this module does not know about specific providers.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use airlock_common::BROWSER_URL_MAX;
use airlock_common::supervisor_capnp::browser;
use url::Url;

/// Maximum number of notices kept per session. Later notices are counted,
/// not stored.
const MAX_NOTICES: usize = 8;

/// Maximum number of open requests per [`SESSION_OPEN_WINDOW`]. The limit
/// also counts requests that the URL checks or the grants refuse later. A
/// sign-in opens one page, and a retry opens one more.
const SESSION_MAX_OPENS: usize = 5;
/// Time window of the open rate limit.
const SESSION_OPEN_WINDOW: Duration = Duration::from_mins(1);

/// Characters that a URL for the opener must not contain, in addition to
/// whitespace and control characters. The opener gets the URL as one
/// argument without a shell. This is a second line of defense for openers
/// that are shell scripts (`xdg-open`).
const FORBIDDEN: &[char] = &[
    '`', '$', ';', '|', '<', '>', '(', ')', '\\', '"', '\'', '{', '}', '*', '!', '^',
];

/// Answer of a grant to a URL that the guest wants to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantAnswer {
    /// The URL is not a page of this grant.
    NotMine,
    /// Open the URL.
    Allow,
    /// The URL is a page of this grant, but it must not open. The reason is
    /// a message for the user, shown as it is. It never contains the query.
    Refuse(String),
}

/// Permission to open some pages in the host browser. The browser asks
/// each grant until one allows the URL. The network services are the
/// grants (see [`crate::services::sign_in::LoopbackSignIn`]). They know
/// their sign-in pages and forward the callback of the page into the guest.
pub trait BrowserGrant {
    /// Answer a URL that passed the checks of the browser.
    /// Args:
    ///  - `url`: Checked URL, see [`checked_url`]
    ///
    /// Returns:
    ///   Answer of the grant. An `Allow` can prepare the use of the page
    ///   (for example, forward its callback port).
    fn allow(&self, url: &Url) -> GrantAnswer;
}

/// Check that a URL is safe to open: `https`, no user name, no explicit
/// port, no fragment, not too long, and no whitespace, control or shell
/// characters.
/// Args:
///  - `raw`: URL from the guest
///
/// Returns:
///   Parsed URL. The opener gets its string form. The error is a message
///   for the user. It never contains the query.
pub fn checked_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| "not a valid URL".to_string())?;
    if url.scheme() != "https" {
        return Err(format!("{}: only https pages open", url.scheme()));
    }
    let place = place_of(&url);
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{place}: the URL carries a user name"));
    }
    if url.port().is_some() {
        return Err(format!("{place}: the URL names a port"));
    }
    if url.fragment().is_some() {
        return Err(format!("{place}: the URL has a fragment"));
    }
    if url.as_str().len() > BROWSER_URL_MAX {
        return Err(format!("{place}: the URL is too long"));
    }
    if url
        .as_str()
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || FORBIDDEN.contains(&c))
    {
        return Err(format!("{place}: the URL has characters airlock refuses"));
    }
    Ok(url)
}

/// Host and path of a URL, for messages and logs. Logs never show the
/// query, because it contains OAuth state.
fn place_of(url: &Url) -> String {
    format!("{}{}", url.host_str().unwrap_or_default(), url.path())
}

/// Find a host program that opens a URL in the default browser.
fn detect_opener() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        return crate::util::on_path("open").then_some("open");
    }
    let display = ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|v| !v.is_empty()));
    (display && crate::util::on_path("xdg-open")).then_some("xdg-open")
}

/// Start `program url` detached: no stdio, its own process group, and a
/// thread that reaps it. No shell: the URL is the only argument.
fn spawn_opener(program: &str, url: &str) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Opens a checked URL on the host.
pub(super) type OpenFn = Box<dyn Fn(&str) -> std::io::Result<()>>;

/// Rate limit: a maximum of `max_opens` open requests per `window`.
pub(super) struct RateLimit {
    /// Maximum number of open requests in one window.
    pub(super) max_opens: usize,
    /// Length of the window.
    pub(super) window: Duration,
}

/// Host side of the browser bridge for one boot. Cheap to clone. All
/// clones share the state.
///
/// Messages for the user wait in a queue ([`Browser::take_notices`]), and
/// the caller shows them after the session: output into a running
/// full-screen tool would corrupt its screen. Each refusal is also logged
/// (warn) when it occurs.
#[derive(Clone)]
pub struct Browser(Rc<BrowserInner>);

struct BrowserInner {
    grants: Vec<Rc<dyn BrowserGrant>>,
    /// `None`: no browser program on this host. The grants still run.
    opener: Option<OpenFn>,
    limit: RateLimit,
    /// Arrival times of the requests in the rate window. Runtime state.
    recent: RefCell<VecDeque<Instant>>,
    /// Messages for the user after the session. Runtime state.
    notices: RefCell<Notices>,
}

/// Queue of messages for the user, and the count of dropped messages.
#[derive(Default)]
struct Notices {
    items: Vec<String>,
    dropped: usize,
}

impl Browser {
    /// Make a browser that opens the pages that the grants allow, with the
    /// browser program of the host.
    /// Args:
    ///  - `grants`: Grants that decide which pages can open
    ///
    /// Returns:
    ///   Browser, or `None` if there are no grants. Then the guest gets no
    ///   browser.
    pub fn new(grants: Vec<Rc<dyn BrowserGrant>>) -> Option<Self> {
        let opener = detect_opener()
            .map(|program| -> OpenFn { Box::new(move |url: &str| spawn_opener(program, url)) });
        let limit = RateLimit {
            max_opens: SESSION_MAX_OPENS,
            window: SESSION_OPEN_WINDOW,
        };
        (!grants.is_empty()).then(|| Self::with_opener(grants, opener, limit))
    }

    /// Make a browser with an explicit opener (`None`: no browser program).
    pub(super) fn with_opener(
        grants: Vec<Rc<dyn BrowserGrant>>,
        opener: Option<OpenFn>,
        limit: RateLimit,
    ) -> Self {
        Self(Rc::new(BrowserInner {
            grants,
            opener,
            limit,
            recent: RefCell::new(VecDeque::new()),
            notices: RefCell::new(Notices::default()),
        }))
    }

    /// Get the queued messages for the user, oldest first. Clears the
    /// queue.
    pub fn take_notices(&self) -> Vec<String> {
        let Notices { mut items, dropped } = self.0.notices.take();
        if dropped > 0 {
            items.push(format!(
                "{dropped} more requests to open a page were refused"
            ));
        }
        items
    }

    /// Queue `msg` for after the session, and log it now.
    fn notice(&self, msg: String) {
        tracing::warn!("browser: {msg}");
        let mut n = self.0.notices.borrow_mut();
        if n.items.len() < MAX_NOTICES {
            n.items.push(msg);
        } else {
            n.dropped += 1;
        }
    }

    /// Handle one open request from the guest: check it, let the grants
    /// decide, and open the page.
    /// Args:
    ///  - `raw`: URL from the guest
    ///
    /// Returns:
    ///   Error message if the request is refused or the browser does not
    ///   start. When the host has no browser program, it queues a notice
    ///   and returns `Ok`.
    pub fn open(&self, raw: &str) -> Result<(), String> {
        // No await occurs between the rate limit and the grant answers.
        // Thus concurrent requests cannot pass either check in parallel.
        let url = self.admit(raw).inspect_err(|msg| {
            self.notice(msg.clone());
        })?;
        let page = place_of(&url);
        let Some(opener) = &self.0.opener else {
            self.notice(format!(
                "no browser program found on this computer; open the sign-in link \
                 for {page} yourself"
            ));
            return Ok(());
        };
        match opener(url.as_str()) {
            Ok(()) => {
                tracing::debug!("browser: opened a page on {page}");
                Ok(())
            }
            Err(e) => {
                let msg = format!("could not start the browser: {e}");
                self.notice(msg.clone());
                Err(msg)
            }
        }
    }

    /// Do the checks of [`Self::open`]: the rate limit, the URL checks, and
    /// then the grants.
    /// Returns:
    ///   URL that a grant allowed, or a refusal message.
    fn admit(&self, raw: &str) -> Result<Url, String> {
        self.count_open()?;
        let url = checked_url(raw).map_err(|r| format!("refused to open a page: {r}"))?;
        let mut refusals = vec![];
        for grant in &self.0.grants {
            match grant.allow(&url) {
                GrantAnswer::Allow => {
                    tracing::info!("browser: allowed a page on {}", place_of(&url));
                    return Ok(url);
                }
                GrantAnswer::Refuse(reason) => refusals.push(reason),
                GrantAnswer::NotMine => {}
            }
        }
        if refusals.is_empty() {
            return Err(format!(
                "refused to open a page: {} is not a sign-in page airlock knows",
                place_of(&url)
            ));
        }
        Err(refusals.join("; "))
    }

    /// Count one open request against the rate limit.
    fn count_open(&self) -> Result<(), String> {
        let RateLimit { max_opens, window } = self.0.limit;
        let mut recent = self.0.recent.borrow_mut();
        let now = Instant::now();
        while recent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= window)
        {
            recent.pop_front();
        }
        if recent.len() >= max_opens {
            return Err("the sandbox asked to open too many pages; refused".into());
        }
        recent.push_back(now);
        Ok(())
    }
}

/// `Browser` RPC capability that the guest gets.
pub struct BrowserImpl {
    browser: Browser,
}

impl BrowserImpl {
    /// Make the capability for `browser`.
    pub fn new(browser: Browser) -> Self {
        Self { browser }
    }
}

impl browser::Server for BrowserImpl {
    async fn open(
        self: Rc<Self>,
        params: browser::OpenParams,
        _results: browser::OpenResults,
    ) -> Result<(), capnp::Error> {
        let url = params.get()?.get_url()?.to_str()?;
        self.browser.open(url).map_err(capnp::Error::failed)
    }
}
