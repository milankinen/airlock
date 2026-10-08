//! Sandbox tab.
//!
//! Shows the terminal of the sandbox process, with its cursor.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

use crate::pty::TuiTerminalSink;

/// Position of the native terminal cursor in the sandbox area.
/// Args:
///  - `sink`: Virtual terminal of the sandbox process
///  - `area`: Screen area of the Sandbox tab body.
///
/// Returns:
///   The (x, y) position, or `None` if the cursor must stay hidden.
pub fn cursor_position(sink: &TuiTerminalSink, area: Rect) -> Option<(u16, u16)> {
    let screen = sink.screen();
    // Hide the cursor in the scrollback view. The vt100 cursor position is
    // for the live screen, not for the scrolled-back view. Thus the real
    // cursor would be at an unrelated cell.
    if screen.hide_cursor() || screen.scrollback() > 0 {
        return None;
    }
    let (cy, cx) = screen.cursor_position();
    let x = area.x + cx;
    let y = area.y + cy;
    if x < area.right() && y < area.bottom() {
        Some((x, y))
    } else {
        None
    }
}

/// Widget that draws the vt100 screen into a ratatui buffer.
pub struct TerminalWidget<'a> {
    sink: &'a TuiTerminalSink,
}

impl<'a> TerminalWidget<'a> {
    /// Create a widget that draws the screen of `sink`.
    pub fn new(sink: &'a TuiTerminalSink) -> Self {
        Self { sink }
    }
}

impl Widget for TerminalWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let screen = self.sink.screen();
        let rows = area.height.min(screen.size().0);
        let cols = area.width.min(screen.size().1);

        for row in 0..rows {
            let mut col = 0;
            while col < cols {
                let x = area.x + col;
                let y = area.y + row;
                if x >= area.right() || y >= area.bottom() {
                    break;
                }

                let Some(cell) = screen.cell(row, col) else {
                    col += 1;
                    continue;
                };

                // Skip continuation cells of wide chars. In ratatui, the wide
                // base cell before it already uses both columns. A write here
                // corrupts the ratatui diff renderer.
                if cell.is_wide_continuation() {
                    col += 1;
                    continue;
                }

                let fg = vt100_color_to_ratatui(cell.fgcolor());
                let bg = vt100_color_to_ratatui(cell.bgcolor());

                let mut style = Style::default().fg(fg).bg(bg);
                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                let buf_cell = &mut buf[(x, y)];
                buf_cell.set_style(style);
                let ch = cell.contents();
                if ch.is_empty() {
                    buf_cell.set_char(' ');
                } else {
                    buf_cell.set_symbol(ch);
                }

                col += if cell.is_wide() { 2 } else { 1 };
            }
        }
    }
}

fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
