//! Mouse input on the Sandbox tab, with and without guest mouse mode.

use crossterm::event::{KeyCode, MouseButton, MouseEventKind};

use crate::test_cfg::Tui;

/// Turns on guest mouse mode: press and release reports (1000) in SGR
/// format (1006).
const SGR_PRESS_RELEASE: &[u8] = b"\x1b[?1000h\x1b[?1006h";

/// Test that mouse events go to the sandbox as SGR reports when the sandbox
/// program turns on mouse mode.
///   1. Turn on mouse mode from the sandbox
///   2. Press and release the left button and scroll up
///   3. Check the SGR reports, with cells counted from 1
#[test]
fn click_with_guest_mouse_mode_is_forwarded_as_guest_cell() {
    let mut tui = Tui::new();
    tui.output(SGR_PRESS_RELEASE);

    tui.click((4, 2));
    tui.mouse(MouseEventKind::Up(MouseButton::Left), (4, 2));
    tui.mouse(MouseEventKind::ScrollUp, (0, 0));

    assert_eq!(tui.sent_bytes(), b"\x1b[<0;5;3M\x1b[<0;5;3m\x1b[<64;1;1M");
}

/// Test that a click on the tab bar changes the tab also in mouse mode. The
/// tab bar belongs to the TUI, not to the sandbox.
///   1. Turn on mouse mode from the sandbox
///   2. Click the Monitor tab
///   3. Check that the sandbox got nothing and the Monitor tab shows
#[test]
fn click_on_tab_bar_with_guest_mouse_mode_switches_tab() {
    let mut tui = Tui::new();
    tui.output(SGR_PRESS_RELEASE);

    tui.click_text("Monitor");

    assert!(tui.sent().is_empty());
    assert!(tui.screen().contains("airlock sandbox monitor (1.2.3)"));
}

/// Test that a click without mouse mode does not go to the sandbox and shows
/// a hint about text selection.
///   1. Check that the status row has no hint
///   2. Click in the body
///   3. Check that the sandbox got nothing and the hint shows
#[test]
fn click_without_guest_mouse_mode_shows_select_hint_and_is_not_forwarded() {
    let mut tui = Tui::new();
    assert!(!tui.row(29).contains("to select text"));

    tui.click((4, 2));

    assert!(tui.sent().is_empty());
    assert!(tui.row(29).contains("to select text"));
}

/// Test that the wheel scrolls a scrolled-back view to the live screen
/// before wheel events go to the sandbox in mouse mode.
///   1. Print 100 lines and scroll up, then turn on mouse mode
///   2. Scroll down and check that the live screen shows and the sandbox got
///      nothing
///   3. Scroll up and check that the sandbox got a wheel report and the view
///      did not move
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

/// Test that mouse events on the Monitor tab do not go to the sandbox, also
/// in mouse mode.
///   1. Turn on mouse mode and open the Monitor tab
///   2. Click and scroll
///   3. Check that the sandbox got nothing
#[test]
fn mouse_on_monitor_tab_is_not_forwarded() {
    let mut tui = Tui::new();
    tui.output(SGR_PRESS_RELEASE);
    tui.key(KeyCode::F(2));

    tui.click((4, 10));
    tui.mouse(MouseEventKind::ScrollUp, (4, 10));

    assert!(tui.sent().is_empty());
}
