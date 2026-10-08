//! Clipboard bridge between container processes and the host clipboard:
//! the shims, the FIFOs and the copy and paste loops.

mod test_copy;
mod test_paste;

use std::path::PathBuf;

use super::{COPY_FIFO, PASTE_FIFO, shims};
use crate::test_cfg::BridgeDir;

/// A clipboard bridge in a temp directory. It has a FIFO for each granted
/// direction and all four shims, which point to these FIFOs.
struct Clipboard {
    dir: BridgeDir,
    copy_fifo: PathBuf,
    paste_fifo: PathBuf,
}

impl Clipboard {
    /// Create the bridge files for the granted directions. The FIFO path of
    /// a direction that is not granted stays empty.
    fn install(copy: bool, paste: bool) -> Self {
        let dir = BridgeDir::new();
        let copy_fifo = if copy {
            dir.fifo("clipboard.copy")
        } else {
            dir.path("clipboard.copy")
        };
        let paste_fifo = if paste {
            dir.fifo("clipboard.paste")
        } else {
            dir.path("clipboard.paste")
        };
        for (name, body) in shims(copy, paste) {
            dir.shim(
                name,
                &body,
                &[(COPY_FIFO, &copy_fifo), (PASTE_FIFO, &paste_fifo)],
            );
        }
        Self {
            dir,
            copy_fifo,
            paste_fifo,
        }
    }

    /// Return the path of the shim with the tool name `name`.
    fn tool(&self, name: &str) -> PathBuf {
        self.dir.path(name)
    }
}
