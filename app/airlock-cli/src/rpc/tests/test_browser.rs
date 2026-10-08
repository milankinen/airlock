//! Tests for the guest requests to open a page in the host browser: grants,
//! URL checks, rate limits and the notices for the user.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::supervisor_capnp::browser;
use url::Url;

use crate::rpc::browser::{Browser, BrowserGrant, BrowserImpl, GrantAnswer, RateLimit};
use crate::test_cfg::{block_on_local, rpc_loopback};

/// A real Claude sign-in URL. Its query has `state=`, which must not show
/// in notices.
const CLAUDE_URL: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A35527%2Fcallback&scope=user%3Ainference&code_challenge=Zm9vYmFyYmF6cXV4&code_challenge_method=S256&state=c3RhdGVzdGF0ZQ";

/// A grant that allows pages on `claude.com` and refuses pages on
/// `refused.example`. It does not decide for other hosts.
struct ClaudeGrant;

impl BrowserGrant for ClaudeGrant {
    fn allow(&self, url: &Url) -> GrantAnswer {
        match url.host_str() {
            Some("claude.com") => GrantAnswer::Allow,
            Some("refused.example") => GrantAnswer::Refuse("sign-in is off".into()),
            _ => GrantAnswer::NotMine,
        }
    }
}

/// A guest browser client, served over RPC by a host browser with the
/// grant [`ClaudeGrant`] and a limit of `max_opens` per `window`.
/// Args:
///  - `opener`: `false` makes a host without a browser program
///
/// Returns:
///   The client, the host browser and the URLs that the host opener got.
fn guest_browser(
    max_opens: usize,
    window: Duration,
    opener: bool,
) -> (browser::Client, Browser, Rc<RefCell<Vec<String>>>) {
    let opened = Rc::new(RefCell::new(Vec::new()));
    let log = opened.clone();
    let browser = Browser::with_opener(
        vec![Rc::new(ClaudeGrant)],
        opener.then(|| -> crate::rpc::browser::OpenFn {
            Box::new(move |url: &str| {
                log.borrow_mut().push(url.to_string());
                Ok(())
            })
        }),
        RateLimit { max_opens, window },
    );
    let client = rpc_loopback(
        capnp_rpc::new_client::<browser::Client, _>(BrowserImpl::new(browser.clone())).client,
    );
    (client, browser, opened)
}

/// Ask the host to open `url`. Returns the RPC error message on refusal.
async fn open(client: &browser::Client, url: &str) -> Result<(), String> {
    let mut request = client.open_request();
    request.get().set_url(url);
    request
        .send()
        .promise
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Test that the guest can open only pages that a grant allows, and that
/// each refusal tells the user why without the secret query values.
///   1. Open an allowed sign-in page
///   2. Open a refused page, a page on an unknown host and a URL with shell
///      characters, and check each error
///   3. Check that the host opened only the allowed page
///   4. Check that there is one notice per refusal and no notice has the
///      `state=` value
#[test]
fn guest_opens_granted_page_and_others_are_refused() {
    block_on_local(async {
        let (client, browser, opened) = guest_browser(10, Duration::from_mins(1), true);

        open(&client, CLAUDE_URL).await.unwrap();
        let refused = CLAUDE_URL.replace("claude.com", "refused.example");
        let err = open(&client, &refused).await.unwrap_err();
        assert!(err.contains("sign-in is off"), "{err}");
        let unknown = CLAUDE_URL.replace("claude.com", "evil.example");
        let err = open(&client, &unknown).await.unwrap_err();
        assert!(err.contains("not a sign-in page airlock knows"), "{err}");
        let err = open(&client, &format!("{CLAUDE_URL}&x=$(id)"))
            .await
            .unwrap_err();
        assert!(err.contains("characters airlock refuses"), "{err}");

        assert_eq!(*opened.borrow(), [CLAUDE_URL]);
        let notices = browser.take_notices();
        assert_eq!(notices.len(), 3, "{notices:?}");
        assert!(notices.iter().all(|n| !n.contains("state=")), "{notices:?}");
    });
}

/// Test that the host opens at most the limit of pages in one window, so
/// that a guest cannot flood the user with browser windows.
///   1. Send 6 open requests at the same time with a limit of 3
///   2. Check that 3 pages open and the refusals say "too many pages"
///   3. Wait for the window to end
///   4. Check that a new open request succeeds
#[test]
fn guest_opens_are_rate_limited_per_window() {
    block_on_local(async {
        let (client, browser, opened) = guest_browser(3, Duration::from_millis(100), true);

        let results = futures::future::join_all((0..6).map(|_| open(&client, CLAUDE_URL))).await;
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 3);
        assert_eq!(opened.borrow().len(), 3);
        assert!(
            browser
                .take_notices()
                .iter()
                .all(|n| n.contains("too many pages"))
        );

        // Wait a bit longer than the 100 ms window, so the 3 earlier opens
        // expire.
        tokio::time::sleep(Duration::from_millis(110)).await;
        open(&client, CLAUDE_URL).await.unwrap();
        assert_eq!(opened.borrow().len(), 4);
    });
}

/// Test that an allowed page succeeds on a host without a browser program,
/// and that a notice tells the user to open the page.
///   1. Open an allowed page on a host without a browser program
///   2. Check that the request succeeds
///   3. Check that one notice names the missing program and the page
#[test]
fn granted_page_without_host_browser_program_asks_user_to_open_it() {
    block_on_local(async {
        let (client, browser, _) = guest_browser(3, Duration::from_mins(1), false);

        open(&client, CLAUDE_URL).await.unwrap();

        let notices = browser.take_notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("no browser program"), "{notices:?}");
        assert!(notices[0].contains("claude.com/cai/oauth/authorize"));
    });
}
