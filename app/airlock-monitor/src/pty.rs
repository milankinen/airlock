//! Virtual terminal backed by a `vt100::Parser`.
//!
//! Process output (stdout/stderr) is fed into the parser, and the resulting
//! screen cells are rendered to the ratatui buffer by the sandbox tab.

/// Wraps a `vt100::Parser` to receive PTY output and expose screen state.
pub struct TuiTerminalSink {
    parser: vt100::Parser,
    csi: CsiRewriter,
}

impl TuiTerminalSink {
    pub fn new(rows: u16, cols: u16, scrollback: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, scrollback as usize),
            csi: CsiRewriter::new(),
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    pub fn write(&mut self, data: &[u8]) {
        let rewritten = self.csi.rewrite(data);
        self.parser.process(&rewritten);
    }

    pub fn scroll_up(&mut self, rows: usize) {
        // Alternate screen (vim, htop, etc.) has no scrollback — scrolling
        // into it would mix alt-screen geometry with normal-screen rows.
        if self.parser.screen().alternate_screen() {
            return;
        }
        let offset = self.parser.screen().scrollback().saturating_add(rows);
        self.parser.screen_mut().set_scrollback(offset);
    }

    pub fn scroll_down(&mut self, rows: usize) {
        if self.parser.screen().alternate_screen() {
            return;
        }
        let offset = self.parser.screen().scrollback().saturating_sub(rows);
        self.parser.screen_mut().set_scrollback(offset);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }
}

/// Streaming rewriter that replaces HVP (`CSI ... f`) with CUP (`CSI ... H`).
///
/// Some terminal applications (notably btop) use HVP — functionally equivalent
/// to CUP per ECMA-48 — but the `vt100` crate only implements CUP and silently
/// ignores the positioning for HVP, which collapses the rendered output onto
/// whatever row the cursor happened to be on.
///
/// Only plain CSI (no private-mode introducers like `?`, `>`, `<`, `=`) with
/// final byte `f` is rewritten; SGR and other sequences are untouched.
struct CsiRewriter {
    state: CsiState,
}

enum CsiState {
    Normal,
    Esc,
    Csi { has_intro: bool },
}

impl CsiRewriter {
    fn new() -> Self {
        Self {
            state: CsiState::Normal,
        }
    }

    fn rewrite(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        for &b in data {
            match self.state {
                CsiState::Normal => {
                    if b == 0x1b {
                        self.state = CsiState::Esc;
                    }
                    out.push(b);
                }
                CsiState::Esc => {
                    if b == b'[' {
                        self.state = CsiState::Csi { has_intro: false };
                    } else {
                        self.state = CsiState::Normal;
                    }
                    out.push(b);
                }
                CsiState::Csi { ref mut has_intro } => {
                    // Private-mode introducer immediately after `[`
                    if matches!(b, b'?' | b'>' | b'<' | b'=') {
                        *has_intro = true;
                        out.push(b);
                    } else if (0x30..=0x3f).contains(&b) {
                        // Parameter byte (digits, ';', ':')
                        out.push(b);
                    } else if (0x20..=0x2f).contains(&b) {
                        // Intermediate byte
                        out.push(b);
                    } else if (0x40..=0x7e).contains(&b) {
                        // Final byte — rewrite HVP to CUP if no private intro
                        let final_byte = if b == b'f' && !*has_intro { b'H' } else { b };
                        out.push(final_byte);
                        self.state = CsiState::Normal;
                    } else {
                        // Malformed; reset and pass through
                        out.push(b);
                        self.state = CsiState::Normal;
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_hvp_to_cup() {
        let mut r = CsiRewriter::new();
        assert_eq!(r.rewrite(b"\x1b[2;3fX"), b"\x1b[2;3HX");
    }

    #[test]
    fn leaves_cup_alone() {
        let mut r = CsiRewriter::new();
        assert_eq!(r.rewrite(b"\x1b[2;3HX"), b"\x1b[2;3HX");
    }

    #[test]
    fn leaves_private_mode_alone() {
        let mut r = CsiRewriter::new();
        // \x1b[?2026h must not have its trailing byte touched.
        assert_eq!(r.rewrite(b"\x1b[?2026h"), b"\x1b[?2026h");
    }

    #[test]
    fn preserves_state_across_chunks() {
        let mut r = CsiRewriter::new();
        let a = r.rewrite(b"\x1b[2;");
        let b = r.rewrite(b"3fX");
        let mut combined = a;
        combined.extend_from_slice(&b);
        assert_eq!(combined, b"\x1b[2;3HX");
    }

    #[test]
    fn literal_f_in_text_untouched() {
        let mut r = CsiRewriter::new();
        assert_eq!(r.rewrite(b"fish"), b"fish");
    }

    #[test]
    fn sink_positions_via_hvp() {
        // Without the rewrite, `vt100` ignores HVP positioning and both writes
        // land on the same row. With the rewrite, they land on their intended
        // rows.
        let mut sink = TuiTerminalSink::new(6, 20, 100);
        sink.write(b"\x1b[2;3HA");
        sink.write(b"\x1b[4;3fB");
        let screen = sink.screen();
        assert_eq!(screen.cell(1, 2).unwrap().contents(), "A");
        assert_eq!(screen.cell(3, 2).unwrap().contents(), "B");
    }
}
