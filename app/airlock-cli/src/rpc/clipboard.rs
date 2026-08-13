//! Host-side clipboard bridge.
//!
//! Serves the `Clipboard` capability the guest calls to reach the host
//! clipboard. The guest has no other route: withholding this object denies
//! access outright, and every call re-checks the per-direction grant and the
//! size cap here rather than trusting anything inside the sandbox.
//!
//! Clipboard programs are spawned with `std::process::Command` on
//! `spawn_blocking`, matching how the rest of the CLI shells out
//! (`crate::oci::docker`), so a wedged clipboard tool cannot stall the
//! single-threaded RPC runtime.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;

use airlock_common::supervisor_capnp::*;

/// A pair of host programs that write and read the system clipboard.
///
/// `write`/`read` are full argv slices — element 0 is the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTool {
    /// Name used in diagnostics.
    pub name: &'static str,
    write: &'static [&'static str],
    read: &'static [&'static str],
    /// Environment variable that must be set for this tool to reach a
    /// display server. `None` for tools that talk to the OS directly.
    ///
    /// This is the *host* side, where a display genuinely may or may not
    /// exist — unlike the guest, where airlock deliberately declines to
    /// invent one.
    requires_env: Option<&'static str>,
}

/// Candidates in preference order. macOS first (its tools are unconditional),
/// then Wayland, then the two X11 options.
const CANDIDATES: &[HostTool] = &[
    HostTool {
        name: "pbcopy",
        write: &["pbcopy"],
        read: &["pbpaste"],
        requires_env: None,
    },
    HostTool {
        name: "wl-copy",
        write: &["wl-copy"],
        read: &["wl-paste", "--no-newline"],
        requires_env: Some("WAYLAND_DISPLAY"),
    },
    HostTool {
        name: "xclip",
        write: &["xclip", "-selection", "clipboard"],
        read: &["xclip", "-selection", "clipboard", "-o"],
        requires_env: Some("DISPLAY"),
    },
    HostTool {
        name: "xsel",
        write: &["xsel", "--clipboard", "--input"],
        read: &["xsel", "--clipboard", "--output"],
        requires_env: Some("DISPLAY"),
    },
];

/// First candidate whose programs are both on `PATH` and whose display
/// requirement (if any) is satisfied. `None` when the host has no usable
/// clipboard, which callers report as a warning rather than a hard failure.
pub fn detect() -> Option<HostTool> {
    CANDIDATES.iter().copied().find(|t| {
        t.requires_env
            .is_none_or(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()))
            && on_path(t.write[0])
            && on_path(t.read[0])
    })
}

/// Whether `program` resolves to an executable file on `PATH`.
fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(program)))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

impl HostTool {
    /// Write `data` to the host clipboard. Blocking — call on a blocking pool.
    fn write(&self, data: &[u8]) -> anyhow::Result<()> {
        let mut child = Command::new(self.write[0])
            .args(&self.write[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        // Scoped so stdin is dropped (and the pipe closed) before `wait`,
        // which would otherwise deadlock against a tool waiting for EOF.
        {
            let mut stdin = child.stdin.take().expect("piped stdin");
            stdin.write_all(data)?;
        }
        let status = child.wait()?;
        if !status.success() {
            anyhow::bail!("{} exited with {status}", self.write[0]);
        }
        Ok(())
    }

    /// Read the host clipboard. Blocking — call on a blocking pool.
    ///
    /// A failing read is reported as an empty clipboard rather than an
    /// error: `wl-paste` exits non-zero when the clipboard is empty, and
    /// the guest program is invariably a `||` fallback chain that copes
    /// with emptiness far better than with a broken pipe.
    fn read(&self) -> Vec<u8> {
        match Command::new(self.read[0])
            .args(&self.read[1..])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => out.stdout,
            Ok(out) => {
                tracing::debug!(
                    "clipboard: {} exited with {} — treating as empty",
                    self.read[0],
                    out.status
                );
                Vec::new()
            }
            Err(e) => {
                tracing::warn!("clipboard: spawning {} failed: {e}", self.read[0]);
                Vec::new()
            }
        }
    }
}

/// Cap'n Proto `Clipboard` server bridging the guest to the host clipboard.
pub struct ClipboardImpl {
    tool: HostTool,
    copy: bool,
    paste: bool,
    /// Largest accepted guest → host transfer, in bytes.
    limit: u64,
}

impl ClipboardImpl {
    pub fn new(tool: HostTool, copy: bool, paste: bool, limit: u64) -> Self {
        Self {
            tool,
            copy,
            paste,
            limit,
        }
    }
}

impl clipboard::Server for ClipboardImpl {
    async fn copy(
        self: Rc<Self>,
        params: clipboard::CopyParams,
        _results: clipboard::CopyResults,
    ) -> Result<(), capnp::Error> {
        // Belt and braces: the capability is not handed over at all when
        // copy is ungranted, so reaching this arm means the guest got hold
        // of an object it should not have.
        if !self.copy {
            return Err(capnp::Error::failed("clipboard copy is not granted".into()));
        }
        let data = params.get()?.get_data()?.to_vec();
        if data.len() as u64 > self.limit {
            tracing::warn!(
                "clipboard: rejected {} byte copy from the sandbox (limit {} bytes)",
                data.len(),
                self.limit
            );
            return Err(capnp::Error::failed(format!(
                "clipboard copy of {} bytes exceeds the {} byte limit",
                data.len(),
                self.limit
            )));
        }

        let tool = self.tool;
        let len = data.len();
        tokio::task::spawn_blocking(move || tool.write(&data))
            .await
            .map_err(|e| capnp::Error::failed(format!("clipboard copy task: {e}")))?
            .map_err(|e| capnp::Error::failed(format!("clipboard copy: {e}")))?;
        tracing::debug!(
            "clipboard: copied {len} bytes from the sandbox via {}",
            tool.name
        );
        Ok(())
    }

    async fn paste(
        self: Rc<Self>,
        _params: clipboard::PasteParams,
        mut results: clipboard::PasteResults,
    ) -> Result<(), capnp::Error> {
        if !self.paste {
            return Err(capnp::Error::failed(
                "clipboard paste is not granted".into(),
            ));
        }

        let tool = self.tool;
        let data = tokio::task::spawn_blocking(move || tool.read())
            .await
            .map_err(|e| capnp::Error::failed(format!("clipboard paste task: {e}")))?;
        tracing::debug!(
            "clipboard: handed {} bytes to the sandbox via {}",
            data.len(),
            tool.name
        );
        results.get().set_data(&data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> HostTool {
        HostTool {
            name: "test",
            write: &["true"],
            read: &["true"],
            requires_env: None,
        }
    }

    /// `sh` is on PATH everywhere we run; a nonsense name is not.
    #[test]
    fn on_path_finds_real_programs() {
        assert!(on_path("sh"));
        assert!(!on_path("airlock-definitely-not-a-real-program"));
    }

    /// A directory named like the program must not count as a hit.
    #[test]
    fn on_path_rejects_directories() {
        assert!(!is_executable(Path::new("/")));
    }

    /// Every candidate must name a program in both directions, so a
    /// half-filled entry can't slip through and fail at call time.
    #[test]
    fn candidates_are_well_formed() {
        for c in CANDIDATES {
            assert!(!c.write.is_empty(), "{} has no write argv", c.name);
            assert!(!c.read.is_empty(), "{} has no read argv", c.name);
        }
    }

    #[test]
    fn write_reports_spawn_failure() {
        let missing = HostTool {
            name: "missing",
            write: &["airlock-definitely-not-a-real-program"],
            read: &["airlock-definitely-not-a-real-program"],
            requires_env: None,
        };
        assert!(missing.write(b"hi").is_err());
    }

    /// A failing read is an empty clipboard, not an error — the guest's
    /// fallback chain handles emptiness but not a broken pipe.
    #[test]
    fn read_failure_is_empty_not_error() {
        let missing = HostTool {
            name: "missing",
            write: &["airlock-definitely-not-a-real-program"],
            read: &["airlock-definitely-not-a-real-program"],
            requires_env: None,
        };
        assert!(missing.read().is_empty());
    }

    #[test]
    fn tool_is_copyable_for_blocking_tasks() {
        // Guards the `spawn_blocking(move || tool.write(..))` calls above,
        // which require HostTool: Copy + Send + 'static.
        fn assert_send_static<T: Send + 'static>(_: T) {}
        assert_send_static(tool());
    }
}
