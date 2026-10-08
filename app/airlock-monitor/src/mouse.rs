//! Mouse event forwarding to the sandboxed program.
//!
//! Converts host mouse events into the input that the sandboxed program expects
//! for its mouse reporting mode.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::pty::{MouseProtocolEncoding, MouseProtocolMode};

/// Xterm button codes. A wheel tick is a button press with bit 6 set. The
/// horizontal wheel continues the same numbering.
const BTN_RELEASE: u8 = 3;
const BTN_WHEEL_UP: u8 = 64;
const BTN_WHEEL_DOWN: u8 = 65;
const BTN_WHEEL_LEFT: u8 = 66;
const BTN_WHEEL_RIGHT: u8 = 67;

/// Value to add to the button code to mark a report as motion, not as a
/// state change.
const MOTION: u8 = 32;

const MOD_SHIFT: u8 = 4;
const MOD_ALT: u8 = 8;
const MOD_CTRL: u8 = 16;

/// Largest coordinate in the legacy encoding. Each byte holds the value
/// plus 32, so the maximum is 223 + 32 = 255.
const LEGACY_MAX_COORD: u16 = 223;
/// Largest coordinate in the UTF-8 encoding. Each value plus 32 is one
/// character of 1 or 2 UTF-8 bytes, so the maximum is 2015 + 32 = 2047.
const UTF8_MAX_COORD: u16 = 2015;

/// Mouse event to report to the guest, independent of the wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Report {
    /// A button was pressed, or the wheel turned.
    Press(u8),
    /// A button was released. Holds the button, because SGR can report it.
    /// The legacy encoding cannot, and reports only [`BTN_RELEASE`].
    Release(u8),
    /// The pointer moved to a different cell with a button held.
    Drag(u8),
    /// The pointer moved to a different cell with no button held.
    Motion,
}

/// Encode a host mouse event for the guest PTY.
/// Args:
///  - `event`: Mouse event from the host terminal
///  - `mode`: Mouse protocol mode that the guest enabled
///  - `encoding`: Mouse report encoding that the guest wants
///  - `body`: Screen rect where the TUI draws the guest's grid. The guest
///    always gets coordinates in its own 1-based grid, independent of the
///    body position in the host terminal.
///
/// Returns:
///   The encoded bytes, or `None` if the event must not go to the guest.
///   `None` means one of these:
///    * the guest has mouse reporting off
///    * the event is outside `body`
///    * the guest's protocol mode does not want this type of event
///    * the legacy encoding cannot express the position.
///
///   For `None`, the caller must handle the event itself.
pub fn encode(
    event: MouseEvent,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
    body: Rect,
) -> Option<Vec<u8>> {
    // The TUI holds the mouse capture of the host terminal. Without the
    // capture, the mouse goes to the host terminal emulator, not to the
    // guest. The guest's own `\e[?1000h` never gets to the host terminal,
    // because the embedded `vt100` parses the guest output. Thus this
    // function makes the mouse bytes for the guest.
    if mode == MouseProtocolMode::None {
        return None;
    }
    let (col, row) = rebase(event.column, event.row, body)?;
    let report = classify(event.kind);
    if !wanted(mode, report) {
        return None;
    }
    let mods = modifier_bits(event.modifiers);
    match encoding {
        MouseProtocolEncoding::Sgr => Some(encode_sgr(report, mods, col, row)),
        MouseProtocolEncoding::Default => encode_legacy(report, mods, col, row, false),
        MouseProtocolEncoding::Utf8 => encode_legacy(report, mods, col, row, true),
    }
}

/// Convert host terminal coordinates into the guest's 1-based grid.
/// Returns:
///   The (column, row) in the guest grid, or `None` if the position is
///   outside `body`.
fn rebase(column: u16, row: u16, body: Rect) -> Option<(u16, u16)> {
    if column < body.x || row < body.y {
        return None;
    }
    let col = column - body.x;
    let line = row - body.y;
    if col >= body.width || line >= body.height {
        return None;
    }
    Some((col + 1, line + 1))
}

/// Convert a crossterm event kind into a [`Report`].
fn classify(kind: MouseEventKind) -> Report {
    match kind {
        MouseEventKind::Down(b) => Report::Press(button_code(b)),
        MouseEventKind::Up(b) => Report::Release(button_code(b)),
        MouseEventKind::Drag(b) => Report::Drag(button_code(b)),
        MouseEventKind::Moved => Report::Motion,
        MouseEventKind::ScrollUp => Report::Press(BTN_WHEEL_UP),
        MouseEventKind::ScrollDown => Report::Press(BTN_WHEEL_DOWN),
        MouseEventKind::ScrollLeft => Report::Press(BTN_WHEEL_LEFT),
        MouseEventKind::ScrollRight => Report::Press(BTN_WHEEL_RIGHT),
    }
}

fn button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// True if the guest's protocol mode wants this type of event. A program
/// that enabled only presses must not get many motion events.
fn wanted(mode: MouseProtocolMode, report: Report) -> bool {
    match report {
        // The caller already rejected `mode == None`.
        Report::Press(_) => true,
        Report::Release(_) => matches!(
            mode,
            MouseProtocolMode::PressRelease
                | MouseProtocolMode::ButtonMotion
                | MouseProtocolMode::AnyMotion
        ),
        Report::Drag(_) => matches!(
            mode,
            MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
        ),
        Report::Motion => mode == MouseProtocolMode::AnyMotion,
    }
}

fn modifier_bits(modifiers: KeyModifiers) -> u8 {
    let mut bits = 0;
    if modifiers.contains(KeyModifiers::SHIFT) {
        bits |= MOD_SHIFT;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        bits |= MOD_ALT;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        bits |= MOD_CTRL;
    }
    bits
}

/// Encode a report in the SGR format (`\e[?1006h`): `CSI < Cb ; Cx ; Cy M`,
/// with a final `m` for a release. The parameters are decimal, so there is
/// no coordinate limit.
fn encode_sgr(report: Report, mods: u8, col: u16, row: u16) -> Vec<u8> {
    let (button, final_byte) = match report {
        Report::Press(b) => (b, b'M'),
        Report::Release(b) => (b, b'm'),
        Report::Drag(b) => (b + MOTION, b'M'),
        Report::Motion => (BTN_RELEASE + MOTION, b'M'),
    };
    let cb = button + mods;
    let mut out = format!("\x1b[<{cb};{col};{row}").into_bytes();
    out.push(final_byte);
    out
}

/// Encode a report in the legacy format (`\e[?1000h`): `CSI M Cb Cx Cy`,
/// with 32 added to each value.
///
/// A release does not identify the button. Without `utf8` (default mode),
/// each value is one raw byte, so coordinates above 223 cannot be encoded.
/// With `utf8` (`\e[?1005h`), each value is one UTF-8 character, so values
/// of 128 or more take 2 bytes and coordinates up to 2015 can be encoded.
/// For a coordinate that the mode cannot encode, the function returns
/// `None`, so that the event does not go to the wrong cell.
fn encode_legacy(report: Report, mods: u8, col: u16, row: u16, utf8: bool) -> Option<Vec<u8>> {
    let max = if utf8 {
        UTF8_MAX_COORD
    } else {
        LEGACY_MAX_COORD
    };
    if col > max || row > max {
        return None;
    }
    let button = match report {
        Report::Press(b) => b,
        Report::Release(_) => BTN_RELEASE,
        Report::Drag(b) => b + MOTION,
        Report::Motion => BTN_RELEASE + MOTION,
    };
    let mut out = Vec::with_capacity(9);
    out.extend_from_slice(b"\x1b[M");
    for value in [u16::from(button + mods), col, row] {
        let value = 32 + value;
        if utf8 {
            // The checks above keep the value below 2048, so it is always
            // a valid character.
            let c = char::from_u32(value.into())?;
            out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        } else {
            out.push(value as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    //! Tests of the encoder that sends host mouse events to the guest.

    use super::*;

    /// Body area of the sandbox terminal. It starts at column 2, row 1.
    const BODY: Rect = Rect {
        x: 2,
        y: 1,
        width: 40,
        height: 20,
    };

    /// Make a mouse event at cell `(column, row)` of the host terminal.
    fn ev(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers,
        }
    }

    /// Encode an event without modifiers in SGR format, as text.
    fn sgr(kind: MouseEventKind, at: (u16, u16), mode: MouseProtocolMode) -> Option<String> {
        sgr_with(ev(kind, at.0, at.1, KeyModifiers::NONE), mode)
    }

    /// Encode `event` in SGR format relative to [`BODY`], as text.
    fn sgr_with(event: MouseEvent, mode: MouseProtocolMode) -> Option<String> {
        encode(event, mode, MouseProtocolEncoding::Sgr, BODY).map(|b| String::from_utf8(b).unwrap())
    }

    /// Test that the SGR encoder gives the correct button code for each event
    /// kind and counts cells from 1 at the top-left corner of the body. The
    /// guest program uses these values to find the button and the cell.
    ///   1. Encode presses, releases, drags, motion and wheel events
    ///   2. Check each report, also at the bottom-right cell of the body
    #[test]
    fn sgr_encodes_buttons_motion_and_wheel_relative_to_body() {
        use MouseEventKind::{
            Down, Drag, Moved, ScrollDown, ScrollLeft, ScrollRight, ScrollUp, Up,
        };
        for (kind, at, mode, expected) in [
            (
                Down(MouseButton::Left),
                (2, 1),
                MouseProtocolMode::PressRelease,
                "\x1b[<0;1;1M",
            ),
            (
                Up(MouseButton::Left),
                (2, 1),
                MouseProtocolMode::PressRelease,
                "\x1b[<0;1;1m",
            ),
            (
                Down(MouseButton::Middle),
                (2, 1),
                MouseProtocolMode::PressRelease,
                "\x1b[<1;1;1M",
            ),
            (
                Down(MouseButton::Right),
                (2, 1),
                MouseProtocolMode::PressRelease,
                "\x1b[<2;1;1M",
            ),
            (
                Drag(MouseButton::Left),
                (4, 3),
                MouseProtocolMode::ButtonMotion,
                "\x1b[<32;3;3M",
            ),
            (Moved, (2, 1), MouseProtocolMode::AnyMotion, "\x1b[<35;1;1M"),
            (
                ScrollUp,
                (2, 1),
                MouseProtocolMode::PressRelease,
                "\x1b[<64;1;1M",
            ),
            (
                ScrollDown,
                (2, 1),
                MouseProtocolMode::PressRelease,
                "\x1b[<65;1;1M",
            ),
            (
                ScrollLeft,
                (2, 1),
                MouseProtocolMode::PressRelease,
                "\x1b[<66;1;1M",
            ),
            (
                ScrollRight,
                (2, 1),
                MouseProtocolMode::PressRelease,
                "\x1b[<67;1;1M",
            ),
            (
                Down(MouseButton::Left),
                (41, 20),
                MouseProtocolMode::PressRelease,
                "\x1b[<0;40;20M",
            ),
        ] {
            assert_eq!(sgr(kind, at, mode).as_deref(), Some(expected), "{kind:?}");
        }
    }

    /// Test that the SGR encoder adds the modifier bits to the button code:
    /// Shift 4, Alt 8 and Ctrl 16.
    ///   1. Encode a left press with each modifier set
    ///   2. Check the button code of each report
    #[test]
    fn sgr_folds_modifiers_into_button_code() {
        let down = MouseEventKind::Down(MouseButton::Left);
        for (modifiers, cb) in [
            (KeyModifiers::SHIFT, 4),
            (KeyModifiers::ALT, 8),
            (KeyModifiers::CONTROL, 16),
            (KeyModifiers::CONTROL | KeyModifiers::ALT, 24),
            (
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
                28,
            ),
        ] {
            assert_eq!(
                sgr_with(ev(down, 2, 1, modifiers), MouseProtocolMode::PressRelease).as_deref(),
                Some(format!("\x1b[<{cb};1;1M").as_str()),
                "{modifiers:?}"
            );
        }
    }

    /// Test that the legacy encoding adds 32 to each value, and drops cells that
    /// one byte cannot hold. Old programs use this encoding.
    ///   1. Encode a press and a release in the legacy format and check the bytes
    ///   2. Check that the UTF-8 encoding gives the same bytes for small cells
    ///   3. Check that a far cell is dropped in the legacy format but not in SGR
    ///      or UTF-8
    #[test]
    fn legacy_encoding_offsets_bytes_and_drops_unrepresentable_cells() {
        let legacy = |kind, column, row, encoding, body| {
            encode(
                ev(kind, column, row, KeyModifiers::NONE),
                MouseProtocolMode::PressRelease,
                encoding,
                body,
            )
        };
        let down = MouseEventKind::Down(MouseButton::Left);
        let up = MouseEventKind::Up(MouseButton::Right);
        let wide = Rect::new(0, 0, 300, 300);

        // Button 0 at body cell (3, 3), each value plus 32.
        assert_eq!(
            legacy(down, 4, 3, MouseProtocolEncoding::Default, BODY).as_deref(),
            Some(b"\x1b[M\x20\x23\x23".as_slice())
        );
        // The legacy format has no button for a release. It sends button 3.
        assert_eq!(
            legacy(up, 2, 1, MouseProtocolEncoding::Default, BODY).as_deref(),
            Some(b"\x1b[M\x23\x21\x21".as_slice())
        );
        assert_eq!(
            legacy(down, 4, 3, MouseProtocolEncoding::Utf8, BODY),
            legacy(down, 4, 3, MouseProtocolEncoding::Default, BODY)
        );
        // Column 251 plus 32 does not fit in one byte.
        assert_eq!(
            legacy(down, 250, 5, MouseProtocolEncoding::Default, wide),
            None
        );
        assert!(legacy(down, 250, 5, MouseProtocolEncoding::Sgr, wide).is_some());
        assert!(legacy(down, 250, 5, MouseProtocolEncoding::Utf8, wide).is_some());
    }

    /// Test that the UTF-8 encoding sends a value of 128 or more as a 2-byte
    /// UTF-8 character. Raw bytes from 0x80 to 0xFF are not valid UTF-8, and
    /// a program in this mode cannot decode them.
    ///   1. Encode a press at column 96 and row 300 in the UTF-8 format
    ///   2. Check that column 96 (value 128) and row 300 (value 332) each
    ///      give 2 bytes
    ///   3. Check that the output is valid UTF-8
    ///   4. Check that a cell above 2015 is dropped
    #[test]
    fn utf8_encoding_sends_large_values_as_utf8_characters() {
        let wide = Rect::new(0, 0, 3000, 3000);
        let press = |column, row| {
            encode(
                ev(
                    MouseEventKind::Down(MouseButton::Left),
                    column,
                    row,
                    KeyModifiers::NONE,
                ),
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Utf8,
                wide,
            )
        };
        // The body starts at 0, so host column 95 is guest column 96.
        let bytes = press(95, 299).unwrap();
        assert_eq!(bytes, b"\x1b[M\x20\xc2\x80\xc5\x8c");
        assert!(String::from_utf8(bytes).is_ok());
        assert_eq!(press(2015, 0), None);
    }

    /// Test that the mouse mode of the guest selects which event kinds go to the
    /// guest. A program must get only the events that it asked for.
    ///   1. Encode a press, release, drag, motion and wheel event in each mode
    ///   2. Check which events give a report
    #[test]
    fn guest_mouse_mode_selects_forwarded_event_classes() {
        use MouseProtocolMode::{AnyMotion, ButtonMotion, None, Press, PressRelease};
        let down = MouseEventKind::Down(MouseButton::Left);
        let up = MouseEventKind::Up(MouseButton::Left);
        let drag = MouseEventKind::Drag(MouseButton::Left);
        let moved = MouseEventKind::Moved;
        let wheel = MouseEventKind::ScrollUp;
        for (mode, forwarded) in [
            (None, [false, false, false, false, false]),
            (Press, [true, false, false, false, true]),
            (PressRelease, [true, true, false, false, true]),
            (ButtonMotion, [true, true, true, false, true]),
            (AnyMotion, [true, true, true, true, true]),
        ] {
            for (kind, want) in [down, up, drag, moved, wheel].into_iter().zip(forwarded) {
                assert_eq!(sgr(kind, (2, 1), mode).is_some(), want, "{mode:?} {kind:?}");
            }
        }
    }

    /// Test that the encoder drops events outside the body. Events on other
    /// parts of the TUI must not go to the guest.
    ///   1. Encode presses left of, above, right of and below the body
    ///   2. Check that no event gives a report
    #[test]
    fn events_outside_body_are_dropped() {
        let down = MouseEventKind::Down(MouseButton::Left);
        for at in [(1, 1), (2, 0), (42, 5), (5, 21), (200, 1)] {
            assert_eq!(
                sgr(down, at, MouseProtocolMode::PressRelease),
                None,
                "{at:?}"
            );
        }
    }
}
