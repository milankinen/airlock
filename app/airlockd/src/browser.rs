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

use std::path::PathBuf;

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
    tokio::task::spawn_local(open_loop(in_rootfs(BROWSER_FIFO), sink));

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
async fn open_loop(path: PathBuf, sink: browser::Client) {
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
    mod test_open_url;

    use super::*;

    #[test]
    fn url_host_drops_userinfo_port_path_query_and_fragment() {
        for (url, host) in [
            (
                "https://claude.com/cai/oauth/authorize?state=s",
                "claude.com",
            ),
            ("http://user:pw@localhost:1455/cb#f", "localhost"),
            ("https://auth.openai.com?x=1", "auth.openai.com"),
        ] {
            assert_eq!(url_host(url), host, "{url}");
        }
    }
}
