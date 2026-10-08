//! Paste from the host clipboard to the container through the clipboard
//! shims.

use std::rc::Rc;

use super::Clipboard;
use crate::clipboard::paste_loop;
use crate::test_cfg::{HostClipboard, run_bridge, run_shim};

/// Start the paste loop for `clipboard`.
fn serve(clipboard: &Clipboard, host: &Rc<HostClipboard>) {
    tokio::task::spawn_local(paste_loop(clipboard.paste_fifo.clone(), host.client()));
}

/// Test that a paste through each paste tool name returns the host
/// clipboard. Programs use different tools, so each name must work.
///   1. Set the host clipboard and start the paste loop
///   2. Paste through wl-paste, xclip and xsel
///   3. Check that each paste returns the host clipboard
///   4. Check that no paste became a copy
#[test]
fn paste_through_every_tool_name_returns_host_clipboard() {
    run_bridge(async {
        let clipboard = Clipboard::install(true, true);
        let host = Rc::new(HostClipboard::default());
        *host.contents.borrow_mut() = Some(b"from host\n".to_vec());
        serve(&clipboard, &host);

        for (tool, args) in [
            ("wl-paste", &[][..]),
            ("xclip", &["-selection", "clipboard", "-o"][..]),
            ("xsel", &["--clipboard", "--output"][..]),
        ] {
            let run = run_shim(&clipboard.tool(tool), args, b"").await;
            assert_eq!(run.code, 0, "{tool}: {run:?}");
            assert_eq!(run.stdout, "from host\n", "{tool}");
        }
        assert!(host.copies.borrow().is_empty());
    });
}

/// Test that a paste that the host refuses gives an empty clipboard and does
/// not hang. The caller must get a clean EOF.
///   1. Start the paste loop with a host that refuses pastes
///   2. Paste through wl-paste and xclip
///   3. Check that each paste exits with code 0 and empty output
#[test]
fn paste_refused_by_host_reads_empty_clipboard_without_hanging() {
    run_bridge(async {
        let clipboard = Clipboard::install(false, true);
        let host = Rc::new(HostClipboard::default());
        serve(&clipboard, &host);

        // wl-paste ignores `-o`. xclip needs it to select paste.
        for tool in ["wl-paste", "xclip"] {
            let run = run_shim(&clipboard.tool(tool), &["-o"], b"").await;
            assert_eq!((run.code, run.stdout.as_str()), (0, ""), "{tool}");
        }
    });
}

/// Test that a paste shim fails at once when paste is not granted. A
/// `cmd-a || cmd-b` chain must try the next tool and not block on a FIFO.
///   1. Install the bridge with only copy granted
///   2. Run each paste tool and check the exit code 1 and the error message
///   3. Check that the paste FIFO does not exist
#[test]
fn paste_shim_without_paste_grant_fails_fast_without_touching_fifo() {
    run_bridge(async {
        let clipboard = Clipboard::install(true, false);

        for (tool, args) in [
            ("wl-paste", &[][..]),
            ("xclip", &["-o"][..]),
            ("xsel", &["--output"][..]),
        ] {
            let run = run_shim(&clipboard.tool(tool), args, b"").await;
            assert_eq!(run.code, 1, "{tool}: {run:?}");
            assert!(
                run.stderr.contains("clipboard paste is not enabled"),
                "{run:?}"
            );
        }
        assert!(!clipboard.paste_fifo.exists());
    });
}
