//! Host-side browser bridge.
//!
//! Serves the `Browser` capability: the guest asks the host to open a URL
//! (a sign-in page) in the user's browser. The VM is untrusted, so
//! [`Browser`] checks every request before it opens anything:
//!
//! - a rate limit on opens;
//! - URL hygiene: `https` only, no user name, no explicit port, no
//!   fragment, a length limit, and no whitespace, control or shell
//!   characters;
//! - then the [`BrowserGrant`]s decide: a page opens only when a grant
//!   allows it. The network services are the grants
//!   ([`crate::services::sign_in::LoopbackSignIn`]); they know their
//!   sign-in pages and forward the page's callback into the guest.
//! - the opener gets the URL as its only argument (no shell), detached.
//!
//! Messages for the user are queued ([`Browser::take_notices`]) and
//! printed after the session: printing into a running full-screen tool
//! would corrupt its screen. Each refusal is also logged (warn) when it
//! happens. Logs name the host and path only; the query carries OAuth
//! state.
//!
//! This module knows no provider: the grants come from the caller.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use airlock_common::BROWSER_URL_MAX;
use airlock_common::supervisor_capnp::browser;
use url::Url;

/// Notices kept per session; later ones are counted, not stored.
const MAX_NOTICES: usize = 8;

/// Opens accepted per [`SESSION_OPEN_WINDOW`]: a sign-in opens one page,
/// a retry another.
const SESSION_MAX_OPENS: usize = 5;
const SESSION_OPEN_WINDOW: Duration = Duration::from_mins(1);

/// Characters refused anywhere in a URL handed to the opener, on top of
/// whitespace and control characters. The opener gets the URL as one
/// argument without a shell; this is a second line of defense for openers
/// that are shell scripts (`xdg-open`).
const FORBIDDEN: &[char] = &[
    '`', '$', ';', '|', '<', '>', '(', ')', '\\', '"', '\'', '{', '}', '*', '!', '^',
];

/// A grant's answer to a URL the guest wants to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantAnswer {
    /// The URL is none of the grant's pages.
    NotMine,
    /// Open the URL.
    Allow,
    /// The URL is the grant's page, but it must not open. The reason is a
    /// message for the user, shown as it is; it never contains the query.
    Refuse(String),
}

/// Who may open which pages in the host's browser. The browser asks every
/// grant until one allows the URL.
pub trait BrowserGrant {
    /// Answer `url`, which passed the browser's own checks. Allowing may
    /// prepare the page's use (e.g. forward its callback port).
    fn allow(&self, url: &Url) -> GrantAnswer;
}

/// Check `raw` for any page: `https`, no user name, port or fragment, not
/// too long, no refused characters. Returns the parsed URL; its string
/// form is what the opener gets. The error is a message for the user; it
/// never contains the query.
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

/// Host and path of a URL, for messages and logs.
fn place_of(url: &Url) -> String {
    format!("{}{}", url.host_str().unwrap_or_default(), url.path())
}

/// A host program that opens a URL in the default browser.
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
/// thread that reaps it.
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
type OpenFn = Box<dyn Fn(&str) -> std::io::Result<()>>;

/// At most `max_opens` open requests per `window`.
struct RateLimit {
    max_opens: usize,
    window: Duration,
}

/// The host's side of the browser bridge for one boot. Cheap to clone;
/// all clones share the state.
#[derive(Clone)]
pub struct Browser(Rc<BrowserInner>);

struct BrowserInner {
    grants: Vec<Rc<dyn BrowserGrant>>,
    /// `None`: no browser program on this host; the grants still run.
    opener: Option<OpenFn>,
    limit: RateLimit,
    /// When the requests inside the rate window arrived. Runtime state.
    recent: RefCell<VecDeque<Instant>>,
    /// Messages for the user after the session. Runtime state.
    notices: RefCell<Notices>,
}

#[derive(Default)]
struct Notices {
    items: Vec<String>,
    dropped: usize,
}

impl Browser {
    /// A browser that opens the pages `grants` allow, with
    /// [`detect_opener`]. `None` without grants: then the guest gets no
    /// browser at all.
    pub fn new(grants: Vec<Rc<dyn BrowserGrant>>) -> Option<Self> {
        let opener = detect_opener()
            .map(|program| -> OpenFn { Box::new(move |url: &str| spawn_opener(program, url)) });
        let limit = RateLimit {
            max_opens: SESSION_MAX_OPENS,
            window: SESSION_OPEN_WINDOW,
        };
        (!grants.is_empty()).then(|| Self::with_opener(grants, opener, limit))
    }

    /// A browser with an explicit opener (`None`: no browser program).
    fn with_opener(
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

    /// The queued messages for the user, oldest first. Clears the queue.
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
    /// decide, and open it. Synchronous from the rate limit to the grants'
    /// answers, so concurrent requests cannot race past either.
    pub fn open(&self, raw: &str) -> Result<(), String> {
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

    /// The checks of [`Self::open`]: the rate limit, the URL hygiene, then
    /// the grants. Returns the URL a grant allowed.
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

/// The `Browser` capability handed to the guest.
pub struct BrowserImpl {
    browser: Browser,
}

impl BrowserImpl {
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::test_support::block_on_local;

    /// `auth-claude.md`: the URL claude passes to `$BROWSER`.
    const CLAUDE_URL: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A35527%2Fcallback&scope=user%3Ainference&code_challenge=Zm9vYmFyYmF6cXV4&code_challenge_method=S256&state=c3RhdGVzdGF0ZQ";

    /// A grant that allows every URL and counts the calls.
    struct AllowAll(Rc<Cell<u32>>);

    impl BrowserGrant for AllowAll {
        fn allow(&self, _url: &Url) -> GrantAnswer {
            self.0.set(self.0.get() + 1);
            GrantAnswer::Allow
        }
    }

    /// A browser over a grant that allows everything, rate-limited to
    /// `max_opens` per `window`. Returns the open and grant counters.
    fn counting_browser(
        max_opens: usize,
        window: Duration,
    ) -> (Browser, Rc<Cell<u32>>, Rc<Cell<u32>>) {
        let opened = Rc::new(Cell::new(0));
        let granted = Rc::new(Cell::new(0));
        let count = opened.clone();
        let browser = Browser::with_opener(
            vec![Rc::new(AllowAll(granted.clone()))],
            Some(Box::new(move |_url: &str| {
                count.set(count.get() + 1);
                Ok(())
            })),
            RateLimit { max_opens, window },
        );
        (browser, opened, granted)
    }

    #[test]
    fn the_rate_limit_holds_under_concurrent_opens() {
        block_on_local(async {
            let (browser, opened, _) = counting_browser(3, Duration::from_mins(1));
            let results = futures::future::join_all((0..6).map(|_| {
                let browser = browser.clone();
                async move { browser.open(CLAUDE_URL) }
            }))
            .await;
            assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 3);
            assert_eq!(opened.get(), 3);
            assert!(
                browser
                    .take_notices()
                    .iter()
                    .all(|n| n.contains("too many pages"))
            );
        });
    }

    /// The rate limit is a window, not a lifetime budget: once the window
    /// has passed, opens are accepted again.
    #[test]
    fn the_rate_limit_window_passes() {
        block_on_local(async {
            let (browser, opened, _) = counting_browser(1, Duration::from_millis(50));
            browser.open(CLAUDE_URL).unwrap();
            assert!(browser.open(CLAUDE_URL).is_err());
            tokio::time::sleep(Duration::from_millis(60)).await;
            browser.open(CLAUDE_URL).unwrap();
            assert_eq!(opened.get(), 2);
        });
    }

    #[test]
    fn no_opener_still_grants() {
        let granted = Rc::new(Cell::new(0));
        let browser = Browser::with_opener(
            vec![Rc::new(AllowAll(granted.clone()))],
            None,
            RateLimit {
                max_opens: 3,
                window: Duration::from_mins(1),
            },
        );
        browser.open(CLAUDE_URL).unwrap();
        assert_eq!(granted.get(), 1);
        assert!(browser.take_notices()[0].contains("no browser program"));
    }
}
