//! The Sandbox tab as a terminal: output, keys, paste, scrollback, resize and
//! exit.

use crossterm::event::{KeyCode, KeyModifiers, MouseEventKind};

use crate::test_cfg::Tui;
use crate::{TuiEvent, TuiInputEvent};

/// Write `count` numbered lines to the sandbox terminal.
fn print_lines(tui: &mut Tui, count: usize) {
    for i in 0..count {
        tui.output(format!("line {i}\r\n").as_bytes());
    }
}

/// Test that sandbox output shows on the Sandbox tab, which is the start tab.
///   1. Send shell output to the terminal
///   2. Check the first two rows and the tab bar on the last row
#[test]
fn guest_output_renders_in_sandbox_tab() {
    let mut tui = Tui::new();
    tui.output(b"$ echo hello\r\nhello\r\n$ ");

    assert_eq!(tui.row(0), "$ echo hello");
    assert_eq!(tui.row(1), "hello");
    assert!(tui.row(29).contains("F1 Sandbox"));
}

/// Test that cursor moves (CUP and HVP) put text on the correct rows, also
/// when one escape sequence is split between two output chunks.
///   1. Send output with a cursor move that ends in the middle of the next
///      sequence
///   2. Send the remaining output
///   3. Check that each text is on its row and column
#[test]
fn guest_cursor_moves_with_hvp_split_across_chunks_land_on_their_rows() {
    let mut tui = Tui::new();
    // `ESC[r;cH` and `ESC[r;cf` move the cursor to row r, column c (from 1).
    tui.output(b"\x1b[2;3HA\x1b[4;");
    tui.output(b"3fB\x1b[6;1Hfish");

    assert_eq!(tui.row(1), "  A");
    assert_eq!(tui.row(3), "  B");
    assert_eq!(tui.row(5), "fish");
}

/// Test that keys on the Sandbox tab go to the sandbox as terminal bytes.
///   1. Type text, Enter, Ctrl+C, Up and Shift+Enter
///   2. Check the bytes that the sandbox got
#[test]
fn typing_on_sandbox_tab_forwards_encoded_keys_to_guest() {
    let mut tui = Tui::new();
    tui.type_keys("ls");
    tui.key(KeyCode::Enter);
    tui.key_with(KeyCode::Char('c'), KeyModifiers::CONTROL);
    tui.key(KeyCode::Up);
    tui.key_with(KeyCode::Enter, KeyModifiers::SHIFT);

    // Without the kitty keyboard protocol, Shift+Enter is a plain `\r`.
    assert_eq!(tui.sent_bytes(), b"ls\r\x03\x1b[A\r");
}

/// Test that Shift+Enter keeps its modifier when the host terminal has the
/// kitty keyboard protocol. Programs can use Shift+Enter for a new line.
///   1. Turn on the kitty keyboard protocol
///   2. Press Shift+Enter and Enter
///   3. Check that Shift+Enter is `CSI 13;2u` and Enter is `\r`
#[test]
fn shift_enter_with_kitty_keyboard_keeps_its_modifier() {
    let mut tui = Tui::new();
    tui.kitty = true;
    tui.key_with(KeyCode::Enter, KeyModifiers::SHIFT);
    tui.key(KeyCode::Enter);

    assert_eq!(tui.sent_bytes(), b"\x1b[13;2u\r");
}

/// Test that a paste has bracketed paste markers only while the sandbox
/// program turns on bracketed paste mode.
///   1. Paste and check that the text has no markers
///   2. Turn on the mode from the sandbox, paste, and check the markers
///   3. Turn off the mode, paste, and check that the text has no markers
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

/// Test that a paste on the Monitor tab does not go to the sandbox. Text that
/// the user pastes there must not run as a command.
///   1. Open the Monitor tab
///   2. Paste a command and check that the sandbox got nothing
#[test]
fn paste_on_monitor_tab_is_not_forwarded() {
    let mut tui = Tui::new();
    tui.key(KeyCode::F(2));
    tui.paste("rm -rf /\n");

    assert!(tui.sent().is_empty());
}

/// Test that the mouse wheel scrolls the scrollback and that a key press goes
/// back to the live screen.
///   1. Print 100 lines and scroll up, then check the view and that the
///      sandbox got nothing
///   2. Scroll down and check that the live screen shows again
///   3. Scroll up and type a key
///   4. Check that the live screen shows and that the key went to the
///      sandbox
#[test]
fn wheel_scrolls_back_through_history_and_typing_returns_to_live_screen() {
    let mut tui = Tui::new();
    print_lines(&mut tui, 100);
    // The body has 28 rows, and the last row is empty after the final
    // newline. One wheel step scrolls three lines.
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

/// Test that the mouse wheel does not scroll while a full-screen program
/// uses the alternate screen.
///   1. Print 100 lines, then switch to the alternate screen
///   2. Scroll up and check that the alternate screen still shows
#[test]
fn wheel_on_alternate_screen_does_not_scroll() {
    let mut tui = Tui::new();
    print_lines(&mut tui, 100);
    tui.output(b"\x1b[?1049h\x1b[H~ vim");

    tui.mouse(MouseEventKind::ScrollUp, (10, 10));

    assert_eq!(tui.row(0), "~ vim");
}

/// Test that a resize sends the body size to the sandbox, and that a
/// terminal that is too small sends nothing and does not break the TUI.
///   1. Resize to 90x40 and check that the sandbox got 38 rows, 90 columns
///   2. Resize to 2 rows and check that the sandbox got nothing
///   3. Send output, resize to 10 rows and check the new size and the output
#[test]
fn resize_sends_body_size_to_guest_and_ignores_too_small_terminal() {
    let mut tui = Tui::new();
    // The body is two rows smaller than the terminal.
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

/// Test that the TUI exits with the exit code of the sandbox process.
///   1. Send output, then the exit event with code 3
///   2. Check that the TUI returns 3
#[test]
fn sandbox_exit_ends_tui_with_its_code() {
    let mut tui = Tui::new();
    tui.output(b"bye\r\n");

    assert_eq!(tui.send(TuiEvent::Exit(3)), Some(3));
}
