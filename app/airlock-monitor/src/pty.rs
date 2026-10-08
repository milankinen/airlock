//! Virtual terminal for the sandbox process output.
//!
//! Keeps the terminal screen state of the sandbox process. The Sandbox tab
//! shows this screen.

/// Mouse reporting state of the guest, as the parser keeps it. Re-exported
/// so that other modules can use it without `vt100` imports.
pub use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// Virtual terminal that receives PTY output and gives the screen state.
///
/// The Sandbox tab draws the screen cells of this terminal.
pub struct TuiTerminalSink {
    // Process output (stdout and stderr) goes into this parser.
    parser: vt100::Parser,
    csi: CsiRewriter,
}

impl TuiTerminalSink {
    /// Create a terminal.
    /// Args:
    ///  - `rows`: Number of screen rows
    ///  - `cols`: Number of screen columns
    ///  - `scrollback`: Number of scrollback rows to keep.
    pub fn new(rows: u16, cols: u16, scrollback: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, scrollback as usize),
            csi: CsiRewriter::new(),
        }
    }

    /// Current screen state.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Xterm mouse protocol that the guest enabled (`\e[?9h`, `\e[?1000h`,
    /// `\e[?1002h`, `\e[?1003h`). `None` means that the guest did not enable
    /// mouse reporting. Then forwarded events show as literal escape bytes at
    /// its prompt.
    pub fn mouse_protocol_mode(&self) -> MouseProtocolMode {
        self.parser.screen().mouse_protocol_mode()
    }

    /// Mouse report encoding that the guest wants: SGR (`\e[?1006h`), UTF-8
    /// (`\e[?1005h`), or the default single-byte form.
    pub fn mouse_protocol_encoding(&self) -> MouseProtocolEncoding {
        self.parser.screen().mouse_protocol_encoding()
    }

    /// Number of rows that the view is scrolled back from the live screen.
    ///
    /// Zero means that the rows on the screen are the same as the guest's
    /// own grid. Mouse coordinates go to the guest only in this state.
    pub fn scrollback(&self) -> usize {
        self.parser.screen().scrollback()
    }

    /// Change the screen size to `rows` x `cols`. Both must be non-zero.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Process a chunk of PTY output.
    pub fn write(&mut self, data: &[u8]) {
        let rewritten = self.csi.rewrite(data);
        self.parser.process(&rewritten);
    }

    /// Scroll the view back by `rows` rows. No effect on the alternate
    /// screen.
    pub fn scroll_up(&mut self, rows: usize) {
        // The alternate screen (vim, htop, etc.) has no scrollback. A scroll
        // there mixes the alternate screen layout with normal screen rows.
        if self.parser.screen().alternate_screen() {
            return;
        }
        let offset = self.parser.screen().scrollback().saturating_add(rows);
        self.parser.screen_mut().set_scrollback(offset);
    }

    /// Scroll the view forward by `rows` rows, toward the live screen. No
    /// effect on the alternate screen.
    pub fn scroll_down(&mut self, rows: usize) {
        if self.parser.screen().alternate_screen() {
            return;
        }
        let offset = self.parser.screen().scrollback().saturating_sub(rows);
        self.parser.screen_mut().set_scrollback(offset);
    }

    /// Go back to the live screen.
    pub fn scroll_to_bottom(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }
}

/// Streaming rewriter that replaces HVP (`CSI ... f`) with CUP (`CSI ... H`).
///
/// Some terminal applications (for example btop) use HVP. ECMA-48 defines
/// HVP with the same function as CUP. But the `vt100` crate implements only
/// CUP, and silently ignores the position in HVP. Then all output shows on
/// the row where the cursor was.
///
/// The rewriter changes only plain CSI sequences (no private-mode
/// introducers such as `?`, `>`, `<`, `=`) with the final byte `f`. SGR and
/// other sequences stay the same.
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
                    // Private-mode introducer. It usually comes immediately
                    // after `[`, but the check accepts it at any position.
                    if matches!(b, b'?' | b'>' | b'<' | b'=') {
                        *has_intro = true;
                        out.push(b);
                    } else if (0x30..=0x3f).contains(&b) {
                        // Parameter byte (digits, ';', ':').
                        out.push(b);
                    } else if (0x20..=0x2f).contains(&b) {
                        // Intermediate byte.
                        out.push(b);
                    } else if (0x40..=0x7e).contains(&b) {
                        // Final byte. Change HVP to CUP if there is no
                        // private-mode introducer.
                        let final_byte = if b == b'f' && !*has_intro { b'H' } else { b };
                        out.push(final_byte);
                        self.state = CsiState::Normal;
                    } else {
                        // Malformed sequence. Reset the state and keep the byte.
                        out.push(b);
                        self.state = CsiState::Normal;
                    }
                }
            }
        }
        out
    }
}
