//! Vertical histogram for the CPU and memory panels.
//!
//! Shows a history of percent values as bars.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Draw a vertical histogram with one column for each history sample.
/// Args:
///  - `area`: Area to draw in
///  - `history`: Samples in percent (0..=100), oldest first. If there are
///    more samples than columns, only the newest samples show.
///  - `color`: Color of the bars
///  - `buf`: Buffer to draw into.
///
/// The bar height has a precision of 1/8 of a cell, so short bars look
/// smooth. A non-empty history always shows a thin baseline (`▁`), so the
/// widget is visible also at 0%.
pub fn render(area: Rect, history: &[u8], color: Color, buf: &mut Buffer) {
    let cols = area.width as usize;
    if cols == 0 || area.height == 0 || history.is_empty() {
        return;
    }
    // Show the newest `cols` samples. Align them to the right, so new data
    // shows at the right edge.
    let start = history.len().saturating_sub(cols);
    let visible = &history[start..];
    let offset = cols - visible.len();

    let height_eighths = u32::from(area.height) * 8;
    let style = Style::default().fg(color);

    for (i, &pct) in visible.iter().enumerate() {
        let x = area.x + (offset + i) as u16;
        let mut fill = (u32::from(pct) * height_eighths + 50) / 100;
        // Always show at least the lowest sub-cell, so 0% stays visible.
        if fill == 0 {
            fill = 1;
        }
        let full_rows = (fill / 8) as u16;
        let partial = (fill % 8) as u8;

        for r in 0..full_rows {
            let y = area.y + area.height - 1 - r;
            buf.set_string(x, y, "█", style);
        }
        if partial > 0 && full_rows < area.height {
            let y = area.y + area.height - 1 - full_rows;
            buf.set_string(x, y, BLOCKS[partial as usize].to_string(), style);
        }
    }
}
