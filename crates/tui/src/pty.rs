//! Virtual terminal backed by a `vt100::Parser`.
//!
//! Process output (stdout/stderr) is fed into the parser, and the resulting
//! screen cells are rendered to the ratatui buffer by the sandbox tab.

/// Wraps a `vt100::Parser` to receive PTY output and expose screen state.
pub struct TuiTerminalSink {
    parser: vt100::Parser,
}

impl TuiTerminalSink {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 1000),
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    pub fn write(&mut self, data: &[u8]) {
        self.parser.process(data);
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
