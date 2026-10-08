//! Clipboard bridge.
//!
//! Gives container processes access to the host clipboard through the usual
//! clipboard programs `wl-copy`, `wl-paste`, `xclip` and `xsel`. Thus no
//! program in the sandbox must know about the bridge. The host decides if
//! copy, paste or both are available.
//!
//! Each call of a clipboard program is one clipboard operation. The data of
//! two concurrent copies does not mix.

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use airlock_common::supervisor_capnp::clipboard;
use tracing::{debug, info, warn};

use crate::bridge::{in_rootfs, install_shim, make_fifo, make_fifo_at, read_capped};

// Paths as the container sees them. In the default container `PATH`,
// `/usr/local/bin` comes before `/usr/bin` and `/bin` (see `DEFAULT_PATH` in
// the host's `oci.rs`). Thus the shims have priority over programs with the
// same names in these image directories. An image can set its own `PATH`.

/// FIFO for copies (guest to host).
const COPY_FIFO: &str = "/run/airlock/clipboard.copy";
/// FIFO for pastes (host to guest).
const PASTE_FIFO: &str = "/run/airlock/clipboard.paste";
/// Directory of the shim scripts.
const BIN_DIR: &str = "/usr/local/bin";

/// Clipboard grant received in `Supervisor.boot()`.
pub struct ClipboardConfig {
    /// Copy (guest to host) is granted.
    pub copy: bool,
    /// Paste (host to guest) is granted.
    pub paste: bool,
    /// Host clipboard capability. `None` when the host granted neither
    /// direction. Without it, there is no route to the host clipboard.
    pub sink: Option<clipboard::Client>,
    /// Maximum bytes per copy. The host does the real check. This limit
    /// stops a hostile writer that tries to grow the guest buffer without
    /// limit.
    pub limit: u64,
}

impl ClipboardConfig {
    /// Return `true` if the bridge has something to serve. A grant with a
    /// direction but no sink is not a grant.
    fn granted(&self) -> bool {
        self.sink.is_some() && (self.copy || self.paste)
    }
}

/// Start the clipboard bridge: create the FIFOs and shims, then start the
/// serve loops.
///
/// Does nothing if the host granted nothing. Then there are no FIFOs and no
/// shims, and a program that looks for a clipboard tool finds none.
/// Args:
///  - `cfg`: Clipboard grant from the host
///  - `uid`, `gid`: Container user and group that own the FIFOs
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
        tokio::task::spawn_local(copy_loop(in_rootfs(COPY_FIFO), sink.clone(), cfg.limit));
    }
    if cfg.paste {
        tokio::task::spawn_local(paste_loop(in_rootfs(PASTE_FIFO), sink));
    }

    info!(
        "clipboard: bridge ready (copy={}, paste={})",
        cfg.copy, cfg.paste
    );
    Ok(())
}

/// Shim scripts to install, as `(filename, contents)`.
///
/// Installs all four names if one or both directions are granted. Different
/// programs use different tools, and one program can use more than one. For
/// example, Claude Code copies with `wl-copy`, but for paste it tries
/// `xclip` first and then `wl-paste`.
///
/// A shim for a direction that is not granted exits with a non-zero code. It
/// does not wait. Thus a `cmd-a || cmd-b` chain tries the next program and
/// does not block forever on a FIFO that nobody serves.
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

    // `-o`/`--output` selects paste for both xclip and xsel. All other calls
    // are a copy. This is sufficient for the flags that programs use. It is
    // also safe: an unknown call copies, and does not leak clipboard data.
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

/// Serve copies (guest to host).
///
/// Each iteration is one cycle: open, read to EOF, send to host. When the
/// writer closes the FIFO, the clipboard operation ends.
async fn copy_loop(path: PathBuf, sink: clipboard::Client, limit: u64) {
    // The loop is serial on purpose. Two concurrent `wl-copy` calls wait in
    // a queue, so their bytes do not mix into one paste.
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

        // The host applies the size limit and checks the grant again. A
        // rejection comes as a capnp error and must not stop the loop.
        let mut req = sink.copy_request();
        req.get().set_data(&data);
        match req.send().promise.await {
            Ok(_) => debug!("clipboard: forwarded {} bytes to the host", data.len()),
            Err(e) => warn!("clipboard: host rejected a {} byte copy: {e}", data.len()),
        }
    }
}

/// Serve pastes (host to guest).
///
/// The open of the write end blocks until a container process opens the
/// read end. Only then the loop gets the host clipboard. Thus the host
/// clipboard is read on demand, not polled and cached.
async fn paste_loop(path: PathBuf, sink: clipboard::Client) {
    loop {
        let p = path.clone();
        let opened = tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new().write(true).open(&p)?;
            renew_fifo(&p)?;
            Ok::<_, std::io::Error>(file)
        })
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

        // A close without a write still gives a clean EOF. Thus the caller
        // sees a refused paste as an empty clipboard and does not hang. If
        // the reader closes the FIFO during the write, the write gets EPIPE.
        // This is expected, not an error.
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

/// Replace the FIFO at `path` with a new FIFO with the same owner.
///
/// A FIFO has no boundary between two pastes. If the loop opens the same
/// FIFO again before the reader of the last paste sees end of file, that
/// reader stays connected and gets the clipboard again. A reader that has
/// the old FIFO open keeps it, but no new reader can open it. So the close
/// of the write end always gives that reader end of file, and the next
/// paste goes to a new reader.
fn renew_fifo(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::metadata(path)?;
    let mut new = path.as_os_str().to_owned();
    new.push(".new");
    let new = PathBuf::from(new);
    make_fifo_at(&new, meta.uid(), meta.gid()).map_err(std::io::Error::other)?;
    // The rename replaces the path in one step, so a new reader always
    // finds a FIFO.
    std::fs::rename(&new, path)
}

#[cfg(test)]
mod tests;
