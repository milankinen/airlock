use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::supervisor_capnp::browser;
use url::Url;

use crate::rpc::browser::{Browser, BrowserGrant, BrowserImpl, GrantAnswer, RateLimit};
use crate::test_cfg::{block_on_local, rpc_loopback};

const CLAUDE_URL: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A35527%2Fcallback&scope=user%3Ainference&code_challenge=Zm9vYmFyYmF6cXV4&code_challenge_method=S256&state=c3RhdGVzdGF0ZQ";

/// Allows pages on `claude.com`, refuses those on `refused.example`.
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

/// A guest's browser client over a host browser with the grant
/// [`ClaudeGrant`] and `max_opens` per `window`. Returns the URLs the host
/// opener got.
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

        tokio::time::sleep(Duration::from_millis(110)).await;
        open(&client, CLAUDE_URL).await.unwrap();
        assert_eq!(opened.borrow().len(), 4);
    });
}

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
