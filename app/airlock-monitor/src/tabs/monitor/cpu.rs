//! CPU panel of the Monitor tab.
//!
//! Shows usage bars for each CPU core (as in btop), the load average and a
//! history of the mean usage.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

/// Maximum number of samples in the mean-usage history.
const HISTORY_CAPACITY: usize = 120;

/// Number of rows for the total-usage histogram below the core bars.
const HISTOGRAM_ROWS: u16 = 4;

/// State of the CPU panel: the latest CPU snapshot and a short history of
/// the mean usage of all cores, for the histogram.
#[derive(Default)]
pub struct CpuState {
    /// Usage of each core in percent. Empty until the first snapshot.
    pub per_core: Vec<u8>,
    /// Load average for 1, 5 and 15 minutes, if known.
    pub load_avg: Option<(f32, f32, f32)>,
    /// Mean-usage samples, oldest first. At most [`HISTORY_CAPACITY`]
    /// samples.
    pub history: Vec<u8>,
}

impl CpuState {
    /// Create an empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the core usage and load average, and add the current mean usage
    /// to the history. Called one time for each `pollStats` snapshot.
    /// Args:
    ///  - `per_core`: Usage of each core in percent
    ///  - `load_avg`: Load average for 1, 5 and 15 minutes, if known.
    pub fn set_snapshot(&mut self, per_core: Vec<u8>, load_avg: Option<(f32, f32, f32)>) {
        self.per_core = per_core;
        self.load_avg = load_avg;
        if self.history.len() >= HISTORY_CAPACITY {
            self.history.remove(0);
        }
        self.history.push(self.mean());
    }

    /// Mean usage of all cores in percent (0..=100). Zero if there is no
    /// data.
    pub fn mean(&self) -> u8 {
        if self.per_core.is_empty() {
            0
        } else {
            let sum: u32 = self.per_core.iter().map(|&v| u32::from(v)).sum();
            u8::try_from(sum / self.per_core.len() as u32).unwrap_or(0)
        }
    }

    /// Number of rows that the CPU box needs for its current content.
    ///
    /// The callers limit the box height to this value, so that the box does
    /// not expand to fill the terminal.
    pub fn desired_height(&self) -> u16 {
        // Two border rows, one row for each core, one load-average row and
        // the fixed histogram rows.
        let cores = u16::try_from(self.per_core.len()).unwrap_or(0);
        let load = u16::from(self.load_avg.is_some());
        let content = cores + load + HISTOGRAM_ROWS;
        content.saturating_add(2).max(5)
    }
}

/// Widget that draws the CPU panel.
///
/// Each core has one row, for example `c0  ████▌···  42%`. The bar uses
/// half-blocks (`▌`) for half-cell precision. The bar and the percent value
/// use the same color, which changes with the usage (green, yellow, orange,
/// red). The core rows come first. When the box has rows left, a
/// load-average row and then the mean-usage histogram show below them.
pub struct CpuWidget<'a> {
    state: &'a CpuState,
}

impl<'a> CpuWidget<'a> {
    /// Create a widget that draws `state`.
    pub fn new(state: &'a CpuState) -> Self {
        Self { state }
    }
}

impl Widget for CpuWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title = Line::from(Span::styled(
            " cpu ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        let right = Line::from(Span::styled(
            format!(" {}% ", self.state.mean()),
            Style::default().fg(Color::DarkGray),
        ))
        .alignment(Alignment::Right);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(title)
            .title(right);

        let inner = block.inner(area);
        block.render(area, buf);

        if inner.height == 0 || inner.width < 8 {
            return;
        }

        render_body(inner, self.state, buf);
    }
}

fn render_body(area: Rect, state: &CpuState, buf: &mut Buffer) {
    if state.per_core.is_empty() {
        Paragraph::new(Line::from(Span::styled(
            "awaiting stats…",
            Style::default().fg(Color::DarkGray),
        )))
        .render(area, buf);
        return;
    }

    // One char of empty space on each side of each row.
    if area.width < 4 {
        return;
    }
    let content = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width - 2,
        height: area.height,
    };

    // Give rows in this order: core bars, then the load row, then the
    // histogram. A later section gets space only if rows remain. The load
    // row and the histogram stay at the bottom, so extra rows go between
    // them and the core bars.
    let cores = u16::try_from(state.per_core.len()).unwrap_or(u16::MAX);
    let visible_cores = cores.min(content.height);
    let mut free = content.height - visible_cores;
    let load_rows = u16::from(state.load_avg.is_some() && free > 0);
    free -= load_rows;
    let histogram_rows = HISTOGRAM_ROWS.min(free);
    let core_rows = content.height - load_rows - histogram_rows;
    let visible = usize::from(visible_cores);

    for (i, &pct) in state.per_core.iter().take(visible).enumerate() {
        let row = Rect {
            x: content.x,
            y: content.y + i as u16,
            width: content.width,
            height: 1,
        };
        render_core_row(row, i, pct, buf);
    }

    if load_rows == 1
        && let Some((one, five, fifteen)) = state.load_avg
    {
        let row = Rect {
            x: content.x,
            y: content.y + core_rows,
            width: content.width,
            height: 1,
        };
        let line = Line::from(vec![Span::styled(
            format!("load {one:.2} {five:.2} {fifteen:.2}"),
            Style::default().fg(Color::DarkGray),
        )]);
        Paragraph::new(line).render(row, buf);
    }

    if histogram_rows > 0 && !state.history.is_empty() {
        let hist = Rect {
            x: content.x,
            y: content.y + core_rows + load_rows,
            width: content.width,
            height: histogram_rows,
        };
        super::histogram::render(hist, &state.history, color_for(state.mean()), buf);
    }
}

fn render_core_row(row: Rect, idx: usize, pct: u8, buf: &mut Buffer) {
    // Layout: "cNN " (4), then the bar (fills the space), then " PPP%" (5).
    let label = format!("c{idx:<2} ");
    let tail = format!(" {pct:>3}%");

    let label_w = label.chars().count() as u16;
    let tail_w = tail.chars().count() as u16;
    if row.width <= label_w + tail_w + 1 {
        return;
    }
    let bar_w = row.width - label_w - tail_w;

    let label_rect = Rect {
        x: row.x,
        y: row.y,
        width: label_w,
        height: 1,
    };
    Paragraph::new(Line::from(Span::styled(
        label,
        Style::default().fg(Color::DarkGray),
    )))
    .render(label_rect, buf);

    let bar_rect = Rect {
        x: row.x + label_w,
        y: row.y,
        width: bar_w,
        height: 1,
    };
    render_bar(bar_rect, pct, buf);

    let tail_rect = Rect {
        x: row.x + label_w + bar_w,
        y: row.y,
        width: tail_w,
        height: 1,
    };
    Paragraph::new(Line::from(Span::styled(
        tail,
        Style::default().fg(color_for(pct)),
    )))
    .render(tail_rect, buf);
}

/// Draw a horizontal bar filled to `pct` percent into `area`. The fill color
/// is the same as the color of the percent value, so each row looks like one
/// unit.
fn render_bar(area: Rect, pct: u8, buf: &mut Buffer) {
    let cells = u32::from(area.width);
    if cells == 0 {
        return;
    }
    let half_cells = (u32::from(pct) * cells * 2 + 50) / 100;
    let full = (half_cells / 2) as u16;
    let half = (half_cells % 2) as u16;
    let fg = Style::default().fg(color_for(pct));
    let bg = Style::default().fg(Color::DarkGray);

    for i in 0..area.width {
        let (ch, style) = if i < full {
            ('█', fg)
        } else if i == full && half == 1 {
            ('▌', fg)
        } else {
            ('·', bg)
        };
        buf.set_string(area.x + i, area.y, ch.to_string(), style);
    }
}

/// Color for a usage percent: green, yellow, orange or red.
fn color_for(pct: u8) -> Color {
    match pct {
        0..=49 => Color::Green,
        50..=69 => Color::Yellow,
        70..=84 => Color::Rgb(255, 140, 0),
        _ => Color::Red,
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the CPU panel layout.

    use super::*;

    /// Draw `state` in a body of `height` rows and return the rows as text.
    fn draw(state: &CpuState, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, 40, height);
        let mut buf = Buffer::empty(area);
        render_body(area, state, &mut buf);
        (0..height)
            .map(|y| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    /// Test that a short CPU box gives its rows to the core bars first, then
    /// to the load row, then to the histogram. The core bars are the main
    /// content of the box.
    ///   1. Set a snapshot with 3 cores and a load average
    ///   2. Draw in 3 rows and check that all rows are core bars
    ///   3. Draw in 4 rows and check the 3 core bars and the load row
    ///   4. Draw in 10 rows and check that the load row and the histogram
    ///      are at the bottom
    #[test]
    fn short_cpu_box_gives_rows_to_core_bars_first() {
        let mut state = CpuState::new();
        state.set_snapshot(vec![100, 100, 100], Some((1.0, 2.0, 3.0)));

        let rows = draw(&state, 3);
        assert!(
            rows.iter()
                .zip(["c0", "c1", "c2"])
                .all(|(r, c)| r.contains(c)),
            "{rows:#?}"
        );

        let rows = draw(&state, 4);
        assert!(rows[2].contains("c2"), "{rows:#?}");
        assert!(rows[3].contains("load 1.00 2.00 3.00"), "{rows:#?}");

        let rows = draw(&state, 10);
        assert!(rows[2].contains("c2"), "{rows:#?}");
        assert!(rows[5].contains("load"), "{rows:#?}");
        assert!(rows[9].trim() != "", "{rows:#?}");
    }
}
