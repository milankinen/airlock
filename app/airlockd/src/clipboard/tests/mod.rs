mod test_copy;
mod test_paste;

use std::path::PathBuf;

use super::{COPY_FIFO, PASTE_FIFO, shims};
use crate::test_cfg::BridgeDir;

/// A clipboard bridge laid out in a temp dir: the FIFOs of the granted
/// directions and all four shims, pointed at them.
struct Clipboard {
    dir: BridgeDir,
    copy_fifo: PathBuf,
    paste_fifo: PathBuf,
}

impl Clipboard {
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

    fn tool(&self, name: &str) -> PathBuf {
        self.dir.path(name)
    }
}
