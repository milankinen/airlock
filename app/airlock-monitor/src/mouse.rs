//! Re-encoding host mouse events for the sandboxed program.
//!
//! The TUI owns the host terminal's mouse — `EnableMouseCapture` is what
//! makes any event arrive at all — so a guest program that asked for mouse
//! reporting never sees a click or a wheel tick. Merely dropping capture
//! would hand the mouse to the host terminal emulator, not to the guest;
//! the guest's own `\e[?1000h` never reaches the host terminal because its
//! output is parsed by the embedded `vt100` and rendered as cells.
//!
//! So this module is the mirror image of `key_to_bytes`: it turns a
//! crossterm [`MouseEvent`] back into the wire bytes the guest asked for.
//! Pure and terminal-free, so every byte layout below is unit-testable.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::pty::{MouseProtocolEncoding, MouseProtocolMode};

/// How much of the mouse is passed through to the sandboxed program
/// while the Sandbox tab is active.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MousePassthrough {
    /// Nothing is passed through: the TUI handles every mouse event
    /// itself, so the wheel scrolls its own scrollback and a click drops
    /// capture for text selection.
    #[default]
    None,
    /// Every event inside the sandbox body is re-encoded into the guest
    /// PTY — but only while the guest actually has mouse reporting
    /// enabled, so the TUI's own handling stays reachable at a plain
    /// shell prompt.
    All,
}

/// Xterm button codes. Wheel ticks are reported as button presses with
/// bit 6 set; the horizontal wheel continues the same numbering.
const BTN_RELEASE: u8 = 3;
const BTN_WHEEL_UP: u8 = 64;
const BTN_WHEEL_DOWN: u8 = 65;
const BTN_WHEEL_LEFT: u8 = 66;
const BTN_WHEEL_RIGHT: u8 = 67;

/// Added to the button code to mark a report as motion rather than a
/// state change.
const MOTION: u8 = 32;

const MOD_SHIFT: u8 = 4;
const MOD_ALT: u8 = 8;
const MOD_CTRL: u8 = 16;

/// Largest coordinate the legacy encoding can express: each byte carries
/// the value offset by 32, so 223 + 32 = 255 is the ceiling.
const LEGACY_MAX_COORD: u16 = 223;

/// What the guest is told happened, independent of wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Report {
    /// A button went down, or the wheel turned.
    Press(u8),
    /// A button came up. Carries which one, because SGR can say — the
    /// legacy encoding can't and reports a bare [`BTN_RELEASE`].
    Release(u8),
    /// The pointer crossed a cell boundary with a button held.
    Drag(u8),
    /// The pointer crossed a cell boundary with no button held.
    Motion,
}

/// Encode a host mouse event for the guest PTY, or return `None` when it
/// must not be forwarded.
///
/// `None` means one of: the guest has mouse reporting off, the event fell
/// outside `body`, the guest's protocol mode doesn't want this class of
/// event, or the position is beyond what the legacy encoding can express.
/// In every case the caller should fall back to its own handling.
///
/// `body` is the on-screen rect the guest's grid is drawn into;
/// coordinates are rebased onto it so the guest always sees its own
/// 1-based grid regardless of where the body sits in the host terminal.
pub fn encode(
    event: MouseEvent,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
    body: Rect,
) -> Option<Vec<u8>> {
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
        // UTF-8 mode only diverges from the default encoding past column
        // 223 — exactly where `encode_legacy` bails out anyway — so the
        // two share one implementation.
        MouseProtocolEncoding::Default | MouseProtocolEncoding::Utf8 => {
            encode_legacy(report, mods, col, row)
        }
    }
}

/// Translate host-terminal coordinates into the guest's 1-based grid,
/// rejecting anything outside the body rect.
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

/// Whether the guest's protocol mode asked for this class of event. A
/// program that only requested presses must not be handed motion spam.
fn wanted(mode: MouseProtocolMode, report: Report) -> bool {
    match report {
        // `mode == None` is rejected before we get here.
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

/// SGR (`\e[?1006h`): `CSI < Cb ; Cx ; Cy M`, with a final `m` for
/// release. Decimal parameters, so there's no coordinate ceiling.
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

/// Legacy (`\e[?1000h` without an encoding extension): `CSI M Cb Cx Cy`
/// with every byte offset by 32. Release loses the button identity, and
/// coordinates past 223 can't be expressed at all — those events are
/// dropped rather than sent to the wrong cell.
fn encode_legacy(report: Report, mods: u8, col: u16, row: u16) -> Option<Vec<u8>> {
    if col > LEGACY_MAX_COORD || row > LEGACY_MAX_COORD {
        return None;
    }
    let button = match report {
        Report::Press(b) => b,
        Report::Release(_) => BTN_RELEASE,
        Report::Drag(b) => b + MOTION,
        Report::Motion => BTN_RELEASE + MOTION,
    };
    let mut out = Vec::with_capacity(6);
    out.extend_from_slice(b"\x1b[M");
    out.push(32 + button + mods);
    out.push(32 + col as u8);
    out.push(32 + row as u8);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Body rect with a non-zero origin, so every test also exercises
    /// coordinate rebasing rather than an accidental identity mapping.
    const BODY: Rect = Rect {
        x: 2,
        y: 1,
        width: 40,
        height: 20,
    };

    fn ev(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn with_mods(kind: MouseEventKind, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: 2,
            row: 1,
            modifiers,
        }
    }

    fn sgr(event: MouseEvent, mode: MouseProtocolMode) -> Option<String> {
        encode(event, mode, MouseProtocolEncoding::Sgr, BODY)
            .map(|b| String::from_utf8(b).expect("SGR output is ASCII"))
    }

    #[test]
    fn sgr_press_release_and_drag() {
        let down = MouseEventKind::Down(MouseButton::Left);
        let up = MouseEventKind::Up(MouseButton::Left);
        let drag = MouseEventKind::Drag(MouseButton::Left);
        // (2,1) is the body origin, so the guest sees its own cell (1,1).
        assert_eq!(
            sgr(ev(down, 2, 1), MouseProtocolMode::PressRelease).as_deref(),
            Some("\x1b[<0;1;1M")
        );
        // Release uses the final `m` and keeps the button identity.
        assert_eq!(
            sgr(ev(up, 2, 1), MouseProtocolMode::PressRelease).as_deref(),
            Some("\x1b[<0;1;1m")
        );
        // Drag adds the motion bit (32) to the held button.
        assert_eq!(
            sgr(ev(drag, 4, 3), MouseProtocolMode::ButtonMotion).as_deref(),
            Some("\x1b[<32;3;3M")
        );
    }

    #[test]
    fn sgr_middle_right_and_bare_motion() {
        let middle = MouseEventKind::Down(MouseButton::Middle);
        let right = MouseEventKind::Down(MouseButton::Right);
        assert_eq!(
            sgr(ev(middle, 2, 1), MouseProtocolMode::PressRelease).as_deref(),
            Some("\x1b[<1;1;1M")
        );
        assert_eq!(
            sgr(ev(right, 2, 1), MouseProtocolMode::PressRelease).as_deref(),
            Some("\x1b[<2;1;1M")
        );
        // Bare motion is "no button" (3) plus the motion bit.
        assert_eq!(
            sgr(
                ev(MouseEventKind::Moved, 2, 1),
                MouseProtocolMode::AnyMotion
            )
            .as_deref(),
            Some("\x1b[<35;1;1M")
        );
    }

    #[test]
    fn sgr_wheel_both_axes() {
        for (kind, cb) in [
            (MouseEventKind::ScrollUp, 64),
            (MouseEventKind::ScrollDown, 65),
            (MouseEventKind::ScrollLeft, 66),
            (MouseEventKind::ScrollRight, 67),
        ] {
            assert_eq!(
                sgr(ev(kind, 2, 1), MouseProtocolMode::PressRelease).as_deref(),
                Some(format!("\x1b[<{cb};1;1M").as_str()),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn modifier_bits_fold_into_cb() {
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
                sgr(with_mods(down, modifiers), MouseProtocolMode::PressRelease).as_deref(),
                Some(format!("\x1b[<{cb};1;1M").as_str()),
                "{modifiers:?}"
            );
        }
    }

    #[test]
    fn legacy_byte_layout() {
        let down = MouseEventKind::Down(MouseButton::Left);
        let bytes = encode(
            ev(down, 4, 3),
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Default,
            BODY,
        );
        // Cell (3,3) of the guest grid: 32+0, 32+3, 32+3.
        assert_eq!(bytes.as_deref(), Some(b"\x1b[M\x20\x23\x23".as_slice()));
    }

    /// The legacy encoding can't name the released button, so every
    /// release collapses onto code 3.
    #[test]
    fn legacy_release_is_button_three() {
        let up = MouseEventKind::Up(MouseButton::Right);
        let bytes = encode(
            ev(up, 2, 1),
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Default,
            BODY,
        );
        assert_eq!(bytes.as_deref(), Some(b"\x1b[M\x23\x21\x21".as_slice()));
    }

    /// Past column 223 the legacy encoding would silently address the
    /// wrong cell, so the event is dropped instead. SGR has no such limit.
    #[test]
    fn legacy_drops_unrepresentable_coordinates() {
        let wide = Rect {
            x: 0,
            y: 0,
            width: 300,
            height: 300,
        };
        let down = ev(MouseEventKind::Down(MouseButton::Left), 250, 5);
        assert_eq!(
            encode(
                down,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default,
                wide
            ),
            None
        );
        assert!(
            encode(
                down,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr,
                wide
            )
            .is_some()
        );
    }

    /// UTF-8 mode is treated as the legacy encoding: identical below 224,
    /// and above it we drop rather than guess.
    #[test]
    fn utf8_matches_legacy() {
        let down = ev(MouseEventKind::Down(MouseButton::Left), 4, 3);
        assert_eq!(
            encode(
                down,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Utf8,
                BODY
            ),
            encode(
                down,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default,
                BODY
            )
        );
    }

    #[test]
    fn mode_filters_event_classes() {
        let down = ev(MouseEventKind::Down(MouseButton::Left), 2, 1);
        let up = ev(MouseEventKind::Up(MouseButton::Left), 2, 1);
        let drag = ev(MouseEventKind::Drag(MouseButton::Left), 2, 1);
        let moved = ev(MouseEventKind::Moved, 2, 1);
        let wheel = ev(MouseEventKind::ScrollUp, 2, 1);

        // `None` — the guest never asked; nothing is forwarded.
        for e in [down, up, drag, moved, wheel] {
            assert_eq!(sgr(e, MouseProtocolMode::None), None, "{:?}", e.kind);
        }
        // `Press` (DEC 9) — presses and wheel only.
        assert!(sgr(down, MouseProtocolMode::Press).is_some());
        assert!(sgr(wheel, MouseProtocolMode::Press).is_some());
        assert_eq!(sgr(up, MouseProtocolMode::Press), None);
        assert_eq!(sgr(drag, MouseProtocolMode::Press), None);
        assert_eq!(sgr(moved, MouseProtocolMode::Press), None);
        // `PressRelease` (DEC 1000) — adds button up.
        assert!(sgr(up, MouseProtocolMode::PressRelease).is_some());
        assert_eq!(sgr(drag, MouseProtocolMode::PressRelease), None);
        assert_eq!(sgr(moved, MouseProtocolMode::PressRelease), None);
        // `ButtonMotion` (DEC 1002) — adds drag but not bare motion.
        assert!(sgr(drag, MouseProtocolMode::ButtonMotion).is_some());
        assert_eq!(sgr(moved, MouseProtocolMode::ButtonMotion), None);
        // `AnyMotion` (DEC 1003) — everything.
        assert!(sgr(moved, MouseProtocolMode::AnyMotion).is_some());
    }

    #[test]
    fn events_outside_the_body_are_dropped() {
        let down = MouseEventKind::Down(MouseButton::Left);
        for (col, row) in [
            (1, 1),   // left of the body
            (2, 0),   // above the body
            (42, 5),  // right of the body (x 2 + width 40)
            (5, 21),  // below the body (y 1 + height 20)
            (200, 1), // far right
        ] {
            assert_eq!(
                sgr(ev(down, col, row), MouseProtocolMode::PressRelease),
                None,
                "({col},{row}) is outside the body"
            );
        }
        // The far corner of the body is inside, and maps to the guest's
        // bottom-right cell.
        assert_eq!(
            sgr(ev(down, 41, 20), MouseProtocolMode::PressRelease).as_deref(),
            Some("\x1b[<0;40;20M")
        );
    }

    #[test]
    fn passthrough_defaults_to_none() {
        assert_eq!(MousePassthrough::default(), MousePassthrough::None);
    }
}
