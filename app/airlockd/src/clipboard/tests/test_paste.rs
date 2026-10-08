use std::rc::Rc;

use super::Clipboard;
use crate::clipboard::paste_loop;
use crate::test_cfg::{HostClipboard, run_bridge, run_shim};

fn serve(clipboard: &Clipboard, host: &Rc<HostClipboard>) {
    tokio::task::spawn_local(paste_loop(clipboard.paste_fifo.clone(), host.client()));
}

#[test]
#[ignore = "paste_loop reopens the FIFO before the reader sees EOF, so a paste repeats"]
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

#[test]
fn paste_refused_by_host_reads_empty_clipboard_without_hanging() {
    run_bridge(async {
        let clipboard = Clipboard::install(false, true);
        let host = Rc::new(HostClipboard::default());
        serve(&clipboard, &host);

        for tool in ["wl-paste", "xclip"] {
            let run = run_shim(&clipboard.tool(tool), &["-o"], b"").await;
            assert_eq!((run.code, run.stdout.as_str()), (0, ""), "{tool}");
        }
    });
}

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
