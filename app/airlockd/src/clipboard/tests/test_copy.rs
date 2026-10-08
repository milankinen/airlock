use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::rc::Rc;

use super::Clipboard;
use crate::clipboard::{ClipboardConfig, copy_loop, start};
use crate::test_cfg::{HostClipboard, eventually, run_bridge, run_shim};

fn serve(clipboard: &Clipboard, host: &Rc<HostClipboard>, limit: u64) {
    tokio::task::spawn_local(copy_loop(clipboard.copy_fifo.clone(), host.client(), limit));
}

#[test]
fn copy_through_every_tool_name_reaches_host_clipboard() {
    run_bridge(async {
        let clipboard = Clipboard::install(true, true);
        let host = Rc::new(HostClipboard::default());
        serve(&clipboard, &host, 1024);

        for (tool, args) in [
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
            ("xsel", &["--clipboard", "--input"][..]),
        ] {
            let run = run_shim(&clipboard.tool(tool), args, tool.as_bytes()).await;
            assert_eq!(run.code, 0, "{tool}: {run:?}");
        }

        eventually("three copies", || host.copies.borrow().len() == 3).await;
        assert_eq!(host.copied(), ["wl-copy", "xclip", "xsel"]);
        let meta = std::fs::metadata(&clipboard.copy_fifo).unwrap();
        assert!(meta.file_type().is_fifo());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    });
}

#[test]
fn copy_over_limit_or_empty_is_dropped_and_bridge_keeps_serving() {
    run_bridge(async {
        let clipboard = Clipboard::install(true, false);
        let host = Rc::new(HostClipboard::default());
        serve(&clipboard, &host, 16);
        let wl_copy = clipboard.tool("wl-copy");

        for payload in [
            &b"exactly 16 bytes"[..],
            b"seventeen bytes!!",
            &vec![b'z'; 512 * 1024],
            b"",
            b"after",
        ] {
            assert_eq!(run_shim(&wl_copy, &[], payload).await.code, 0);
        }

        eventually("two copies", || host.copies.borrow().len() == 2).await;
        assert_eq!(host.copied(), ["exactly 16 bytes", "after"]);
    });
}

#[test]
fn copy_refused_by_host_does_not_stop_bridge() {
    run_bridge(async {
        let clipboard = Clipboard::install(true, false);
        let host = Rc::new(HostClipboard::default());
        host.refuse_copies.set(1);
        serve(&clipboard, &host, 1024);
        let wl_copy = clipboard.tool("wl-copy");

        run_shim(&wl_copy, &[], b"refused").await;
        run_shim(&wl_copy, &[], b"accepted").await;

        eventually("one copy", || host.copies.borrow().len() == 1).await;
        assert_eq!(host.copied(), ["accepted"]);
    });
}

#[test]
fn copy_shim_without_copy_grant_fails_fast_without_touching_fifo() {
    run_bridge(async {
        let clipboard = Clipboard::install(false, true);

        for (tool, args) in [
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
            ("xsel", &["--input"][..]),
        ] {
            let run = run_shim(&clipboard.tool(tool), args, b"secret").await;
            assert_eq!(run.code, 1, "{tool}: {run:?}");
            assert!(
                run.stderr.contains("clipboard copy is not enabled"),
                "{run:?}"
            );
        }
        assert!(!clipboard.copy_fifo.exists());
    });
}

#[test]
fn grant_without_host_capability_installs_nothing() {
    let cfg = ClipboardConfig {
        copy: true,
        paste: true,
        sink: None,
        limit: 1024,
    };

    assert!(start(cfg, 0, 0).is_ok());
}
