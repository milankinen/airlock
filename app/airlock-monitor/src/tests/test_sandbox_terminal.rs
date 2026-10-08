use crossterm::event::{KeyCode, KeyModifiers, MouseEventKind};

use crate::test_cfg::Tui;
use crate::{TuiEvent, TuiInputEvent};

fn print_lines(tui: &mut Tui, count: usize) {
    for i in 0..count {
        tui.output(format!("line {i}\r\n").as_bytes());
    }
}

#[test]
fn guest_output_renders_in_sandbox_tab() {
    let mut tui = Tui::new();
    tui.output(b"$ echo hello\r\nhello\r\n$ ");

    assert_eq!(tui.row(0), "$ echo hello");
    assert_eq!(tui.row(1), "hello");
    assert!(tui.row(29).contains("F1 Sandbox"));
}

#[test]
fn guest_cursor_moves_with_hvp_split_across_chunks_land_on_their_rows() {
    let mut tui = Tui::new();
    tui.output(b"\x1b[2;3HA\x1b[4;");
    tui.output(b"3fB\x1b[6;1Hfish");

    assert_eq!(tui.row(1), "  A");
    assert_eq!(tui.row(3), "  B");
    assert_eq!(tui.row(5), "fish");
}

#[test]
fn typing_on_sandbox_tab_forwards_encoded_keys_to_guest() {
    let mut tui = Tui::new();
    tui.type_keys("ls");
    tui.key(KeyCode::Enter);
    tui.key_with(KeyCode::Char('c'), KeyModifiers::CONTROL);
    tui.key(KeyCode::Up);
    tui.key_with(KeyCode::Enter, KeyModifiers::SHIFT);

    assert_eq!(tui.sent_bytes(), b"ls\r\x03\x1b[A\r");
}

#[test]
fn shift_enter_with_kitty_keyboard_keeps_its_modifier() {
    let mut tui = Tui::new();
    tui.kitty = true;
    tui.key_with(KeyCode::Enter, KeyModifiers::SHIFT);
    tui.key(KeyCode::Enter);

    assert_eq!(tui.sent_bytes(), b"\x1b[13;2u\r");
}

#[test]
fn paste_is_bracketed_only_while_guest_enables_bracketed_paste() {
    let mut tui = Tui::new();
    tui.paste("a\nb");
    assert_eq!(tui.sent_bytes(), b"a\nb");

    tui.output(b"\x1b[?2004h$ ");
    tui.paste("a\nb");
    assert_eq!(tui.sent_bytes(), b"\x1b[200~a\nb\x1b[201~");

    tui.output(b"\x1b[?2004l");
    tui.paste("c");
    assert_eq!(tui.sent_bytes(), b"c");
}

#[test]
fn paste_on_monitor_tab_is_not_forwarded() {
    let mut tui = Tui::new();
    tui.key(KeyCode::F(2));
    tui.paste("rm -rf /\n");

    assert!(tui.sent().is_empty());
}

#[test]
fn wheel_scrolls_back_through_history_and_typing_returns_to_live_screen() {
    let mut tui = Tui::new();
    print_lines(&mut tui, 100);
    assert_eq!(tui.row(0), "line 73");

    tui.mouse(MouseEventKind::ScrollUp, (10, 10));
    assert_eq!(tui.row(0), "line 70");
    assert!(tui.sent().is_empty());

    tui.mouse(MouseEventKind::ScrollDown, (10, 10));
    assert_eq!(tui.row(0), "line 73");

    tui.mouse(MouseEventKind::ScrollUp, (10, 10));
    tui.type_keys("q");
    assert_eq!(tui.row(0), "line 73");
    assert_eq!(tui.sent_bytes(), b"q");
}

#[test]
fn wheel_on_alternate_screen_does_not_scroll() {
    let mut tui = Tui::new();
    print_lines(&mut tui, 100);
    tui.output(b"\x1b[?1049h\x1b[H~ vim");

    tui.mouse(MouseEventKind::ScrollUp, (10, 10));

    assert_eq!(tui.row(0), "~ vim");
}

#[test]
fn resize_sends_body_size_to_guest_and_ignores_too_small_terminal() {
    let mut tui = Tui::new();
    tui.resize(90, 40);
    let sent = tui.sent();
    assert!(
        matches!(sent[..], [TuiInputEvent::Resize(38, 90)]),
        "{sent:?}"
    );

    tui.resize(90, 2);
    assert!(tui.sent().is_empty());
    tui.output(b"still alive");

    tui.resize(90, 10);
    assert!(matches!(tui.sent()[..], [TuiInputEvent::Resize(8, 90)]));
    assert!(tui.screen().contains("still alive"));
}

#[test]
fn sandbox_exit_ends_tui_with_its_code() {
    let mut tui = Tui::new();
    tui.output(b"bye\r\n");

    assert_eq!(tui.send(TuiEvent::Exit(3)), Some(3));
}
