//! Browser bridge.
//!
//! Lets container processes open web pages in the host browser, for example
//! for a sign-in in the sandbox. Tools that open links through `$BROWSER` use
//! the bridge. The host gives browser access only to the boots that can need
//! it.
//!
//! The bridge exists only for the current boot. No part of it stays on the
//! sandbox disk for later boots.

use std::path::PathBuf;

use airlock_common::supervisor_capnp::browser;
use airlock_common::{BROWSER_FIFO, BROWSER_SHIM, BROWSER_URL_MAX};
use tracing::{debug, info, warn};

use crate::bridge::{in_rootfs, install_shim, make_fifo, read_capped};

/// Maximum bytes read per FIFO open-to-EOF cycle. Concurrent shim calls can
/// share one cycle, so the limit is the size of a few URLs. The loop drops
/// payloads larger than this.
const READ_LIMIT: u64 = 4 * (BROWSER_URL_MAX as u64 + 1);

/// Browser grant received in `Supervisor.boot()`.
pub struct BrowserConfig {
    /// `None` when the host did not grant browser access for this boot.
    pub sink: Option<browser::Client>,
}

/// Start the browser bridge: create the FIFO and shim, then start the serve
/// loop.
///
/// Does nothing if the host did not grant browser access. Then there is no
/// FIFO and no shim. `$BROWSER` (if set) points to nothing, and tools show
/// the link instead.
/// Args:
///  - `cfg`: Browser grant from the host
///  - `uid`, `gid`: Container user and group that own the FIFO
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

/// Make the shim script that writes one URL line to `fifo`.
///
/// For a non-http(s) argument, the shim exits with a non-zero code. Then
/// callers show the link themselves and do not wait for a page that never
/// opens.
fn shim_body(fifo: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # airlock browser shim — asks the host to open a URL.\n\
         case \"${{1:-}}\" in http://*|https://*) ;; *) exit 1 ;; esac\n\
         printf '%s\\n' \"$1\" > {fifo}\n"
    )
}

/// Serve open requests (guest to host).
///
/// Each iteration is one cycle: open, read to EOF, send each http(s) line to
/// the host. The loop is serial and never stops. It logs each failure and
/// starts the next cycle. Thus a hostile writer cannot stop the bridge for
/// later, legitimate calls.
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

        // The guest URL filter is only hygiene. The VM is untrusted, so the
        // host checks every URL again against its own policy.
        for url in urls(text) {
            // Log only the host. The full URL contains OAuth state and the
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

/// Split a FIFO payload into URL lines. Keeps only non-empty http(s) lines
/// of at most [`BROWSER_URL_MAX`] bytes. Removes the whitespace around each
/// line (for example a `\r` from a CRLF writer).
fn urls(payload: &str) -> impl Iterator<Item = &str> {
    payload.lines().map(str::trim).filter(|l| {
        l.len() <= BROWSER_URL_MAX && (l.starts_with("http://") || l.starts_with("https://"))
    })
}

/// Get the host part of an http(s) URL, for logs. Removes userinfo, port,
/// path, query and fragment.
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    host_port.rsplit_once(':').map_or(host_port, |(h, _)| h)
}

#[cfg(test)]
mod tests {
    //! Tests of the browser bridge.

    mod test_open_url;

    use super::*;

    /// Test that the log host of a URL has no userinfo, port, path, query or
    /// fragment. Logs must not show OAuth state or credentials.
    ///   1. Get the host of URLs with these parts
    ///   2. Check that only the host name stays
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
