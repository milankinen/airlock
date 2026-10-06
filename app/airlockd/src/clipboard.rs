//! Guest side of the clipboard bridge.
//!
//! The host hands us a `Clipboard` capability when the project grants a
//! direction. We expose it to container processes as ordinary clipboard
//! programs — `wl-copy`, `wl-paste`, `xclip`, `xsel` — because that is what
//! software actually reaches for; nothing in the sandbox needs to know a
//! bridge exists.
//!
//! **Transport is a FIFO pair, not a unix socket.** A shell shim cannot open
//! a unix socket without `nc`/`socat`, which minimal images do not ship,
//! whereas `cat > fifo` and `cat fifo` need nothing at all.
//!
//! Framing falls out of FIFO semantics: one open-to-EOF cycle is exactly one
//! clipboard operation, so consecutive `wl-copy` invocations cannot run
//! together into one blob.
//!
//! Opening a FIFO blocks — the read end waits for a writer and the write end
//! waits for a reader — so every open happens on the blocking pool. Blocking
//! inline here would wedge the runtime of a process that is PID 1.

use std::io::Write;

use airlock_common::supervisor_capnp::clipboard;
use tracing::{debug, info, warn};

use crate::bridge::{in_rootfs, install_shim, make_fifo, read_capped};

/// Guest-visible paths. `/usr/local/bin` is first on the container `PATH`
/// (see the base env in the host's `oci.rs`), so the shims win over anything
/// the image ships under the same names.
const COPY_FIFO: &str = "/run/airlock/clipboard.copy";
const PASTE_FIFO: &str = "/run/airlock/clipboard.paste";
const BIN_DIR: &str = "/usr/local/bin";

/// Clipboard grant received in `Supervisor.boot()`.
pub struct ClipboardConfig {
    pub copy: bool,
    pub paste: bool,
    /// `None` when the host granted neither direction. Without it there is
    /// no route to the host clipboard at all.
    pub sink: Option<clipboard::Client>,
    /// Max bytes per copy. The host is the real check; this bound stops a
    /// hostile writer growing our buffer without limit.
    pub limit: u64,
}

impl ClipboardConfig {
    /// Nothing to do when the host withheld the capability. Guards against a
    /// grant that claims a direction but carries no sink to serve it.
    fn granted(&self) -> bool {
        self.sink.is_some() && (self.copy || self.paste)
    }
}

/// Create the FIFOs and shims, then spawn the serve loops.
///
/// A no-op when nothing was granted: no FIFOs, no shims, so a program
/// probing for a clipboard tool finds exactly what it would in an ordinary
/// sandbox — nothing.
pub fn start(cfg: ClipboardConfig, uid: u32, gid: u32) -> anyhow::Result<()> {
    if !cfg.granted() {
        debug!("clipboard: not granted, no shims installed");
        return Ok(());
    }
    let sink = cfg.sink.expect("granted() checked sink");

    if cfg.copy {
        make_fifo(COPY_FIFO, uid, gid)?;
    }
    if cfg.paste {
        make_fifo(PASTE_FIFO, uid, gid)?;
    }

    for (name, body) in shims(cfg.copy, cfg.paste) {
        install_shim(&format!("{BIN_DIR}/{name}"), &body)?;
    }

    if cfg.copy {
        tokio::task::spawn_local(copy_loop(sink.clone(), cfg.limit));
    }
    if cfg.paste {
        tokio::task::spawn_local(paste_loop(sink));
    }

    info!(
        "clipboard: bridge ready (copy={}, paste={})",
        cfg.copy, cfg.paste
    );
    Ok(())
}

/// Shim scripts to install, as `(filename, contents)`.
///
/// All four names are installed whenever either direction is granted,
/// because real software disagrees about which to reach for — and even one
/// program can disagree with itself: Claude Code's copy prefers `wl-copy`
/// while its paste tries `xclip` first and falls back to `wl-paste`.
///
/// A shim for an ungranted direction exits non-zero rather than hanging, so
/// the `cmd-a || cmd-b` chains callers typically use move on to the next
/// candidate instead of blocking forever on a FIFO nobody will serve.
fn shims(copy: bool, paste: bool) -> Vec<(&'static str, String)> {
    let copy_branch = if copy {
        format!("exec cat > {COPY_FIFO}")
    } else {
        "echo 'airlock: clipboard copy is not enabled' >&2; exit 1".to_string()
    };
    let paste_branch = if paste {
        format!("exec cat {PASTE_FIFO}")
    } else {
        "echo 'airlock: clipboard paste is not enabled' >&2; exit 1".to_string()
    };

    // `-o`/`--output` selects paste for both xclip and xsel; everything else
    // is a copy. Good enough for the flag sets these are called with, and it
    // fails safe: an unrecognised invocation copies rather than leaking.
    let dispatch = |name: &str| {
        format!(
            "#!/bin/sh\n\
             # airlock clipboard shim ({name}) — bridges to the host clipboard.\n\
             for a in \"$@\"; do\n\
             \tcase \"$a\" in\n\
             \t\t-o|--output) {paste_branch} ;;\n\
             \tesac\n\
             done\n\
             {copy_branch}\n"
        )
    };

    vec![
        (
            "wl-copy",
            format!("#!/bin/sh\n# airlock clipboard shim\n{copy_branch}\n"),
        ),
        (
            "wl-paste",
            format!("#!/bin/sh\n# airlock clipboard shim\n{paste_branch}\n"),
        ),
        ("xclip", dispatch("xclip")),
        ("xsel", dispatch("xsel")),
    ]
}

/// Serve guest → host copies.
///
/// Each iteration is one `open → read to EOF → forward` cycle, which is also
/// the framing: a writer closing the FIFO ends the clipboard operation. The
/// loop is serial on purpose, so two concurrent `wl-copy` calls queue rather
/// than interleaving their bytes into one incoherent paste.
async fn copy_loop(sink: clipboard::Client, limit: u64) {
    let path = in_rootfs(COPY_FIFO);
    loop {
        let p = path.clone();
        let read = tokio::task::spawn_blocking(move || read_capped(&p, limit)).await;

        let data = match read {
            Ok(Ok(Ok(data))) => data,
            Ok(Ok(Err(total))) => {
                warn!(
                    "clipboard: sandbox tried to copy {total} bytes, over the {limit} byte limit — dropped"
                );
                continue;
            }
            Ok(Err(e)) => {
                warn!("clipboard: reading the copy fifo failed: {e}");
                continue;
            }
            Err(e) => {
                warn!("clipboard: copy reader task failed: {e}");
                continue;
            }
        };
        if data.is_empty() {
            continue;
        }

        // The host enforces the size cap and re-checks the grant; a rejection
        // arrives as a capnp error and must not kill the loop.
        let mut req = sink.copy_request();
        req.get().set_data(&data);
        match req.send().promise.await {
            Ok(_) => debug!("clipboard: forwarded {} bytes to the host", data.len()),
            Err(e) => warn!("clipboard: host rejected a {} byte copy: {e}", data.len()),
        }
    }
}

/// Serve host → guest pastes.
///
/// Opening the write end blocks until a container process opens the read end,
/// which is the signal to fetch. Fetching only then means the host clipboard
/// is read on demand rather than polled and cached.
async fn paste_loop(sink: clipboard::Client) {
    let path = in_rootfs(PASTE_FIFO);
    loop {
        let p = path.clone();
        let opened =
            tokio::task::spawn_blocking(move || std::fs::OpenOptions::new().write(true).open(&p))
                .await;

        let file = match opened {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => {
                warn!("clipboard: opening the paste fifo failed: {e}");
                continue;
            }
            Err(e) => {
                warn!("clipboard: paste writer task failed: {e}");
                continue;
            }
        };

        let data = match sink.paste_request().send().promise.await {
            Ok(resp) => match resp
                .get()
                .and_then(clipboard::paste_results::Reader::get_data)
            {
                Ok(d) => d.to_vec(),
                Err(e) => {
                    warn!("clipboard: malformed paste response: {e}");
                    Vec::new()
                }
            },
            Err(e) => {
                warn!("clipboard: host refused a paste: {e}");
                Vec::new()
            }
        };

        // Closing without writing still yields a clean EOF, so a refused
        // paste surfaces to the caller as an empty clipboard rather than a
        // hang. A reader that walks away mid-write gives us EPIPE, which is
        // expected rather than exceptional.
        let n = data.len();
        let write = tokio::task::spawn_blocking(move || {
            let mut file = file;
            file.write_all(&data)
        })
        .await;
        match write {
            Ok(Ok(())) => debug!("clipboard: handed {n} bytes to the sandbox"),
            Ok(Err(e)) => debug!("clipboard: paste reader went away: {e}"),
            Err(e) => warn!("clipboard: paste writer task failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shim(copy: bool, paste: bool, name: &str) -> String {
        shims(copy, paste)
            .into_iter()
            .find(|(n, _)| *n == name)
            .map(|(_, body)| body)
            .expect("shim present")
    }

    /// All four names are always installed — see the note on `shims`.
    #[test]
    fn installs_every_known_tool_name() {
        let names: Vec<_> = shims(true, true).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["wl-copy", "wl-paste", "xclip", "xsel"]);
    }

    #[test]
    fn granted_directions_reach_their_fifo() {
        assert!(shim(true, true, "wl-copy").contains(COPY_FIFO));
        assert!(shim(true, true, "wl-paste").contains(PASTE_FIFO));
    }

    /// An ungranted direction must fail fast rather than block on a FIFO
    /// that nobody serves, or `a || b` fallback chains hang forever.
    #[test]
    fn ungranted_copy_exits_nonzero_without_touching_the_fifo() {
        let s = shim(false, true, "wl-copy");
        assert!(s.contains("exit 1"), "{s}");
        assert!(!s.contains(COPY_FIFO), "{s}");
    }

    #[test]
    fn ungranted_paste_exits_nonzero_without_touching_the_fifo() {
        let s = shim(true, false, "wl-paste");
        assert!(s.contains("exit 1"), "{s}");
        assert!(!s.contains(PASTE_FIFO), "{s}");
    }

    /// xclip/xsel serve both directions, so each must route `-o` to paste
    /// and everything else to copy.
    #[test]
    fn dispatch_shims_route_output_flag_to_paste() {
        for name in ["xclip", "xsel"] {
            let s = shim(true, true, name);
            let o = s.find("-o|--output").expect("dispatch arm");
            let paste = s.find(PASTE_FIFO).expect("paste branch");
            let copy = s.find(COPY_FIFO).expect("copy branch");
            assert!(o < paste, "{name}: -o must select the paste branch");
            assert!(paste < copy, "{name}: copy must be the fallthrough");
        }
    }

    #[test]
    fn every_shim_is_a_sh_script() {
        for (name, body) in shims(true, true) {
            assert!(body.starts_with("#!/bin/sh\n"), "{name} lacks a shebang");
            assert!(body.ends_with('\n'), "{name} lacks a trailing newline");
        }
    }

    /// Withholding the sink must disable the bridge even if the flags claim
    /// a direction — the capability is the grant, not the booleans.
    #[test]
    fn missing_sink_is_never_granted() {
        let cfg = ClipboardConfig {
            copy: true,
            paste: true,
            sink: None,
            limit: 1024,
        };
        assert!(!cfg.granted());
    }
}
