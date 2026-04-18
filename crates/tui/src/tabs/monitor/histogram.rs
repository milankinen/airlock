//! Shared per-column vertical histogram renderer used by the CPU and
//! memory widgets.
//!
//! Each history sample (0..=100) becomes one column. Fill height is
//! rounded to 1/8-block precision so short bars look smooth. A thin
//! baseline (`▁`) is always drawn for non-empty histories so the widget
//! stays visible even at 0%.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub fn render(area: Rect, history: &[u8], color: Color, buf: &mut Buffer) {
    let cols = area.width as usize;
    if cols == 0 || area.height == 0 || history.is_empty() {
        return;
    }
    // Show the most recent `cols` samples, right-aligned so fresh data
    // appears on the right edge.
    let start = history.len().saturating_sub(cols);
    let visible = &history[start..];
    let offset = cols - visible.len();

    let height_eighths = u32::from(area.height) * 8;
    let style = Style::default().fg(color);

    for (i, &pct) in visible.iter().enumerate() {
        let x = area.x + (offset + i) as u16;
        let mut fill = (u32::from(pct) * height_eighths + 50) / 100;
        // Always show at least the lowest sub-cell so 0% remains visible.
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
