//! Guest side of the browser bridge.
//!
//! The host hands us a `Browser` capability only for boots that may need to
//! open a page on the host (an in-VM sign-in). We expose it to container
//! processes as a shim at [`BROWSER_SHIM`]; the host points `$BROWSER` at
//! it, which both Node's `execFile($BROWSER, [url])` and Rust's `webbrowser`
//! crate honour.
//!
//! The shim writes one URL line to a FIFO; a serve loop forwards each http(s)
//! line to the host. The guest filter is only hygiene: the host re-checks
//! every URL against its own policy, because the VM is untrusted.
//!
//! Both paths live on the per-boot `/run/airlock` tmpfs, so no shim is left
//! behind in the persisted rootfs for later boots.

use airlock_common::supervisor_capnp::browser;
use airlock_common::{BROWSER_FIFO, BROWSER_SHIM, BROWSER_URL_MAX};
use tracing::{debug, info, warn};

use crate::bridge::{in_rootfs, install_shim, make_fifo, read_capped};

/// Max bytes read per FIFO open-to-EOF cycle. Concurrent shim calls can share
/// one cycle, so allow a few URLs' worth; anything past this is dropped.
const READ_LIMIT: u64 = 4 * (BROWSER_URL_MAX as u64 + 1);

/// Browser grant received in `Supervisor.boot()`.
pub struct BrowserConfig {
    /// `None` when the host did not grant browser access for this boot.
    pub sink: Option<browser::Client>,
}

/// Create the FIFO and shim, then spawn the serve loop.
///
/// A no-op when not granted: no FIFO, no shim, so `$BROWSER` (if set at all)
/// points at nothing and tools fall back to printing the link.
pub fn start(cfg: BrowserConfig, uid: u32, gid: u32) -> anyhow::Result<()> {
    let Some(sink) = cfg.sink else {
        debug!("browser: not granted, no shim installed");
        return Ok(());
    };

    make_fifo(BROWSER_FIFO, uid, gid)?;
    install_shim(BROWSER_SHIM, &shim_body(BROWSER_FIFO))?;
    tokio::task::spawn_local(open_loop(sink));

    info!("browser: bridge ready");
    Ok(())
}

/// The shim script. Non-http(s) arguments exit non-zero so callers fall back
/// to showing the link themselves instead of waiting on a page that never
/// opens.
fn shim_body(fifo: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # airlock browser shim — asks the host to open a URL.\n\
         case \"${{1:-}}\" in http://*|https://*) ;; *) exit 1 ;; esac\n\
         printf '%s\\n' \"$1\" > {fifo}\n"
    )
}

/// Serve guest → host open requests.
///
/// Each iteration is one `open → read to EOF → forward` cycle. The loop is
/// serial and never exits: every failure is logged and the next cycle
/// starts, so a hostile writer cannot switch the bridge off for later,
/// legitimate calls.
async fn open_loop(sink: browser::Client) {
    let path = in_rootfs(BROWSER_FIFO);
    loop {
        let p = path.clone();
        let read = tokio::task::spawn_blocking(move || read_capped(&p, READ_LIMIT)).await;

        let data = match read {
            Ok(Ok(Ok(data))) => data,
            Ok(Ok(Err(total))) => {
                warn!("browser: dropped a {total} byte write, over the {READ_LIMIT} byte limit");
                continue;
            }
            Ok(Err(e)) => {
                warn!("browser: reading the open fifo failed: {e}");
                continue;
            }
            Err(e) => {
                warn!("browser: open reader task failed: {e}");
                continue;
            }
        };

        let Ok(text) = std::str::from_utf8(&data) else {
            warn!("browser: dropped a non-UTF-8 write");
            continue;
        };

        for url in urls(text) {
            // Log the host only: the full URL carries OAuth state and the
            // PKCE challenge.
            let host = url_host(url);
            let mut req = sink.open_request();
            req.get().set_url(url);
            match req.send().promise.await {
                Ok(_) => debug!("browser: host opened a page on {host}"),
                Err(e) => warn!("browser: host refused a page on {host}: {e}"),
            }
        }
    }
}

/// Split a FIFO payload into URL lines, keeping only non-empty http(s) lines
/// of at most [`BROWSER_URL_MAX`] bytes. Surrounding whitespace (a `\r` from
/// a CRLF writer, say) is trimmed.
fn urls(payload: &str) -> impl Iterator<Item = &str> {
    payload.lines().map(str::trim).filter(|l| {
        l.len() <= BROWSER_URL_MAX && (l.starts_with("http://") || l.starts_with("https://"))
    })
}

/// The host part of an http(s) URL, for logging. Userinfo, port, path, query
/// and fragment are dropped.
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    host_port.rsplit_once(':').map_or(host_port, |(h, _)| h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_is_a_sh_script_writing_to_the_fifo() {
        let s = shim_body(BROWSER_FIFO);
        assert!(s.starts_with("#!/bin/sh\n"), "{s}");
        assert!(s.ends_with('\n'), "{s}");
        assert!(
            s.contains("printf '%s\\n' \"$1\" > /run/airlock/browser.open"),
            "{s}"
        );
    }

    /// Run the real shim body with `sh`, pointed at a plain file instead of
    /// the FIFO, and return (exit status, what it wrote).
    fn run_shim(arg: Option<&str>) -> (i32, Option<String>) {
        static RUN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("airlock-browser-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out");
        let script = dir.join("shim");
        std::fs::remove_file(&out).ok();
        std::fs::write(&script, shim_body(out.to_str().unwrap())).unwrap();

        let mut cmd = std::process::Command::new("sh");
        cmd.arg(&script);
        if let Some(a) = arg {
            cmd.arg(a);
        }
        let status = cmd.status().unwrap().code().unwrap_or(-1);
        let written = std::fs::read_to_string(&out).ok();
        std::fs::remove_dir_all(&dir).ok();
        (status, written)
    }

    #[test]
    fn shim_forwards_http_and_https() {
        for url in ["https://claude.com/x?a=1&b=2", "http://localhost:1455/y"] {
            assert_eq!(run_shim(Some(url)), (0, Some(format!("{url}\n"))), "{url}");
        }
    }

    #[test]
    fn shim_rejects_other_schemes_and_no_argument() {
        for arg in [
            Some("file:///etc/passwd"),
            Some("javascript:alert(1)"),
            Some("-h"),
            Some(""),
            None,
        ] {
            assert_eq!(run_shim(arg), (1, None), "{arg:?}");
        }
    }

    #[test]
    fn urls_keeps_only_http_lines() {
        let payload = "https://a.example/x\r\n\nftp://b.example/\nhttp://c.example\n  \n";
        let got: Vec<_> = urls(payload).collect();
        assert_eq!(got, vec!["https://a.example/x", "http://c.example"]);
    }

    #[test]
    fn urls_drops_oversized_lines() {
        let long = format!("https://a.example/{}", "x".repeat(BROWSER_URL_MAX));
        let payload = format!("{long}\nhttps://ok.example/\n");
        let got: Vec<_> = urls(&payload).collect();
        assert_eq!(got, vec!["https://ok.example/"]);
    }

    #[test]
    fn url_host_strips_everything_but_the_host() {
        assert_eq!(
            url_host("https://claude.com/cai/oauth/authorize?state=s"),
            "claude.com"
        );
        assert_eq!(url_host("http://user:pw@localhost:1455/cb#f"), "localhost");
        assert_eq!(url_host("https://auth.openai.com?x=1"), "auth.openai.com");
    }

    #[test]
    fn missing_sink_is_a_noop() {
        assert!(start(BrowserConfig { sink: None }, 0, 0).is_ok());
    }
}
