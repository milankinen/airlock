//! Browser bridge: URLs from the container go to the host browser through
//! the shim or a raw FIFO write.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::BROWSER_URL_MAX;

use crate::browser::{READ_LIMIT, open_loop, shim_body};
use crate::test_cfg::{BridgeDir, HostBrowser, eventually, run_bridge, run_shim, write_fifo};

/// A running browser bridge in a temp directory, with a fake host browser.
struct Browser {
    _dir: BridgeDir,
    fifo: PathBuf,
    shim: PathBuf,
    host: Rc<HostBrowser>,
}

impl Browser {
    /// Create the FIFO and the shim, and start the open loop.
    fn start() -> Self {
        let dir = BridgeDir::new();
        let fifo = dir.fifo("browser.open");
        let shim = dir.shim("xdg-open", &shim_body(fifo.to_str().unwrap()), &[]);
        let host = Rc::new(HostBrowser::default());
        tokio::task::spawn_local(open_loop(fifo.clone(), host.client()));
        Self {
            _dir: dir,
            fifo,
            shim,
            host,
        }
    }

    /// Wait until the host opened at least `count` URLs. Return all opened
    /// URLs.
    async fn opened_after(&self, count: usize) -> Vec<String> {
        eventually("opened urls", || self.host.opened.borrow().len() >= count).await;
        self.host.opened.borrow().clone()
    }
}

/// Test that the shim sends http and https URLs to the host browser. Sign-in
/// flows in the sandbox need this.
///   1. Start the bridge
///   2. Open an OAuth URL and a localhost callback URL through the shim
///   3. Check that the host opened both URLs in order
#[test]
fn shim_forwards_http_and_https_urls_to_host() {
    run_bridge(async {
        let browser = Browser::start();
        let urls = [
            "https://claude.com/cai/oauth/authorize?state=s&code_challenge=c",
            "http://localhost:1455/callback",
        ];

        for (i, url) in urls.into_iter().enumerate() {
            let run = run_shim(&browser.shim, &[url], b"").await;
            assert_eq!(run.code, 0, "{run:?}");
            // Wait for each URL, so the two shim writes cannot share one
            // FIFO cycle.
            browser.opened_after(i + 1).await;
        }

        assert_eq!(*browser.host.opened.borrow(), urls);
    });
}

/// Test that the shim refuses arguments that are not http(s) URLs and writes
/// nothing to the FIFO. Callers then show the link themselves.
///   1. Run the shim with a file URL, a javascript URL, a flag, an empty
///      argument and no argument
///   2. Check that each run exits with code 1
///   3. Open a valid URL and check that the host opened only this URL
#[test]
fn shim_rejects_non_http_arguments_without_writing_to_fifo() {
    run_bridge(async {
        let browser = Browser::start();

        for args in [
            &["file:///etc/passwd"][..],
            &["javascript:alert(1)"][..],
            &["-h"][..],
            &[""][..],
            &[][..],
        ] {
            assert_eq!(run_shim(&browser.shim, args, b"").await.code, 1, "{args:?}");
        }
        run_shim(&browser.shim, &["https://ok.example/"], b"").await;

        assert_eq!(browser.opened_after(1).await, ["https://ok.example/"]);
    });
}

/// Test that the open loop opens only http(s) lines within the URL limit
/// from a raw FIFO write. The VM is untrusted, so a writer can skip the shim.
///   1. Write one payload with CRLF, empty, ftp, padded, too long and valid
///      lines directly to the FIFO
///   2. Check that the host opened only the http(s) lines, with whitespace
///      removed
#[test]
fn raw_fifo_write_opens_only_http_lines_within_url_limit() {
    run_bridge(async {
        let browser = Browser::start();
        let long = format!("https://long.example/{}", "x".repeat(BROWSER_URL_MAX));
        let payload = format!(
            "https://a.example/x\r\n\nftp://b.example/\n  http://c.example  \n{long}\nhttps://ok.example/\n"
        );

        write_fifo(&browser.fifo, payload.into_bytes()).await;

        assert_eq!(
            browser.opened_after(3).await,
            [
                "https://a.example/x",
                "http://c.example",
                "https://ok.example/"
            ]
        );
    });
}

/// Test that bad writes and host refusals do not stop the open loop. A
/// hostile writer must not block later, legitimate calls.
///   1. Write a payload over the read limit to the FIFO
///   2. Write a payload that is not UTF-8
///   3. Write a URL that the host refuses
///   4. Open a valid URL through the shim and check that the host opened only
///      this URL
#[test]
fn oversized_non_utf8_or_refused_open_does_not_stop_bridge() {
    run_bridge(async {
        let browser = Browser::start();
        browser.host.refuse_opens.set(1);

        let oversized = format!("https://a.example/\n{}", "x".repeat(READ_LIMIT as usize));
        write_fifo(&browser.fifo, oversized.into_bytes()).await;
        // Let the loop finish the cycle, so the next write gets its own cycle.
        tokio::time::sleep(Duration::from_millis(20)).await;
        write_fifo(&browser.fifo, b"https://b.example/\xff\n".to_vec()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        write_fifo(&browser.fifo, b"https://refused.example/\n".to_vec()).await;
        run_shim(&browser.shim, &["https://ok.example/"], b"").await;

        assert_eq!(browser.opened_after(1).await, ["https://ok.example/"]);
    });
}
