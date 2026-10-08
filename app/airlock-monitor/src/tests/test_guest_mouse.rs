use crossterm::event::{KeyCode, MouseButton, MouseEventKind};

use crate::test_cfg::Tui;

const SGR_PRESS_RELEASE: &[u8] = b"\x1b[?1000h\x1b[?1006h";

#[test]
fn click_with_guest_mouse_mode_is_forwarded_as_guest_cell() {
    let mut tui = Tui::new();
    tui.output(SGR_PRESS_RELEASE);

    tui.click((4, 2));
    tui.mouse(MouseEventKind::Up(MouseButton::Left), (4, 2));
    tui.mouse(MouseEventKind::ScrollUp, (0, 0));

    assert_eq!(tui.sent_bytes(), b"\x1b[<0;5;3M\x1b[<0;5;3m\x1b[<64;1;1M");
}

#[test]
fn click_on_tab_bar_with_guest_mouse_mode_switches_tab() {
    let mut tui = Tui::new();
    tui.output(SGR_PRESS_RELEASE);

    tui.click_text("Monitor");

    assert!(tui.sent().is_empty());
    assert!(tui.screen().contains("airlock sandbox monitor (1.2.3)"));
}

#[test]
fn click_without_guest_mouse_mode_shows_select_hint_and_is_not_forwarded() {
    let mut tui = Tui::new();
    assert!(!tui.row(29).contains("to select text"));

    tui.click((4, 2));

    assert!(tui.sent().is_empty());
    assert!(tui.row(29).contains("to select text"));
}

#[test]
fn wheel_in_scrolled_back_view_scrolls_view_until_live_then_forwards() {
    let mut tui = Tui::new();
    for i in 0..100 {
        tui.output(format!("line {i}\r\n").as_bytes());
    }
    tui.mouse(MouseEventKind::ScrollUp, (10, 10));
    tui.output(SGR_PRESS_RELEASE);

    tui.mouse(MouseEventKind::ScrollDown, (10, 10));
    assert!(tui.sent().is_empty());
    assert_eq!(tui.row(0), "line 73");

    tui.mouse(MouseEventKind::ScrollUp, (10, 10));
    assert_eq!(tui.sent_bytes(), b"\x1b[<64;11;11M");
    assert_eq!(tui.row(0), "line 73");
}

#[test]
fn mouse_on_monitor_tab_is_not_forwarded() {
    let mut tui = Tui::new();
    tui.output(SGR_PRESS_RELEASE);
    tui.key(KeyCode::F(2));

    tui.click((4, 10));
    tui.mouse(MouseEventKind::ScrollUp, (4, 10));

    assert!(tui.sent().is_empty());
}
