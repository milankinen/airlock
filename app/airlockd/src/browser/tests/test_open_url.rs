use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::BROWSER_URL_MAX;

use crate::browser::{READ_LIMIT, open_loop, shim_body};
use crate::test_cfg::{BridgeDir, HostBrowser, eventually, run_bridge, run_shim, write_fifo};

struct Browser {
    _dir: BridgeDir,
    fifo: PathBuf,
    shim: PathBuf,
    host: Rc<HostBrowser>,
}

impl Browser {
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

    async fn opened_after(&self, count: usize) -> Vec<String> {
        eventually("opened urls", || self.host.opened.borrow().len() >= count).await;
        self.host.opened.borrow().clone()
    }
}

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
            browser.opened_after(i + 1).await;
        }

        assert_eq!(*browser.host.opened.borrow(), urls);
    });
}

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

#[test]
fn oversized_non_utf8_or_refused_open_does_not_stop_bridge() {
    run_bridge(async {
        let browser = Browser::start();
        browser.host.refuse_opens.set(1);

        let oversized = format!("https://a.example/\n{}", "x".repeat(READ_LIMIT as usize));
        write_fifo(&browser.fifo, oversized.into_bytes()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        write_fifo(&browser.fifo, b"https://b.example/\xff\n".to_vec()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        write_fifo(&browser.fifo, b"https://refused.example/\n".to_vec()).await;
        run_shim(&browser.shim, &["https://ok.example/"], b"").await;

        assert_eq!(browser.opened_after(1).await, ["https://ok.example/"]);
    });
}
