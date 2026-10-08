//! Host-side clipboard bridge.
//!
//! Gives the guest access to the host clipboard, when the project config
//! allows it. The guest has no other access to the host clipboard.

use std::io::Write;
use std::process::{Command, Stdio};
use std::rc::Rc;

use airlock_common::supervisor_capnp::*;

use crate::config::config_values::Clipboard;

/// A pair of host programs that write and read the system clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTool {
    /// Name for diagnostics.
    pub name: &'static str,
    /// Full argv of the write program. Element 0 is the program.
    write: &'static [&'static str],
    /// Full argv of the read program. Element 0 is the program.
    read: &'static [&'static str],
    /// Environment variable that must be set, so this tool can connect to a
    /// display server. `None` for tools that use the OS directly.
    ///
    /// This is the *host* side, where a display can exist or not. In the
    /// guest, airlock intentionally does not make a display.
    requires_env: Option<&'static str>,
}

/// Candidates in order of preference: macOS first (its tools need no
/// display variable), then Wayland, then the two X11 options.
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

/// Find the first candidate that has both programs on `PATH` and its
/// display variable (if any) set. `None` when the host has no usable
/// clipboard.
fn detect() -> Option<HostTool> {
    CANDIDATES.iter().copied().find(|t| {
        t.requires_env
            .is_none_or(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()))
            && crate::util::on_path(t.write[0])
            && crate::util::on_path(t.read[0])
    })
}

impl HostTool {
    /// Write `data` to the host clipboard. Blocks, so call it on a blocking
    /// pool.
    fn write(&self, data: &[u8]) -> anyhow::Result<()> {
        let mut child = Command::new(self.write[0])
            .args(&self.write[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        // Drop stdin (and close the pipe) in this scope, before `wait`.
        // Otherwise `wait` deadlocks with a tool that waits for EOF.
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

    /// Read the host clipboard. Blocks, so call it on a blocking pool.
    /// Returns:
    ///   Clipboard contents. Empty if the read fails.
    fn read(&self) -> Vec<u8> {
        // A failed read gives an empty clipboard, not an error. `wl-paste`
        // exits non-zero when the clipboard is empty. Guest programs usually
        // use a `||` fallback chain, which handles an empty value better
        // than an error.
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

/// Cap'n Proto `Clipboard` server that connects the guest to the host
/// clipboard.
///
/// Each call checks the grant of its direction and the size limit again
/// here. It does not trust anything in the sandbox.
pub struct ClipboardImpl {
    tool: HostTool,
    /// Guest → host copy is granted.
    pub(super) copy: bool,
    /// Host → guest paste is granted.
    pub(super) paste: bool,
    /// Largest accepted guest → host transfer, in bytes.
    pub(super) limit: u64,
}

/// Make the clipboard capability for the `[clipboard]` config.
/// Args:
///  - `config`: Clipboard config of the project
///
/// Returns:
///   Capability, or `None` if the config grants no direction. Then the
///   sandbox has nothing to call. A host without a clipboard program also
///   gives `None`, with a warning. It never causes a failed start.
pub fn for_config(config: &Clipboard) -> Option<ClipboardImpl> {
    if !config.copy && !config.paste {
        return None;
    }
    let Some(tool) = detect() else {
        crate::cli::log!(
            "  {} clipboard disabled: no clipboard program found on the host",
            crate::cli::bullet()
        );
        return None;
    };
    Some(ClipboardImpl {
        tool,
        copy: config.copy,
        paste: config.paste,
        limit: config.copy_limit.0,
    })
}

#[cfg(test)]
impl ClipboardImpl {
    /// A clipboard that writes with the argv `write` and reads with `read`.
    pub(super) fn with_programs(
        write: &'static [&'static str],
        read: &'static [&'static str],
        copy: bool,
        paste: bool,
        limit: u64,
    ) -> Self {
        Self {
            tool: HostTool {
                name: "test",
                write,
                read,
                requires_env: None,
            },
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
        // Defense in depth: the guest gets no capability when copy is not
        // granted. Thus, if the code gets here, the guest has an object
        // that it must not have.
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
        // Run the clipboard program on `spawn_blocking`, as the rest of the
        // CLI does (`crate::oci::docker`). Thus a stuck clipboard tool cannot
        // stop the single-threaded RPC runtime.
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
