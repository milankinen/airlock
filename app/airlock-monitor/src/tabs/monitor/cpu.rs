//! CPU widget — btop-style per-core utilization bars + load average.
//!
//! Each core gets one row: `c0 ▮▮▮▮░░  42%`. The bar uses `▮` / `░`
//! glyphs and half-block `▌` for sub-cell precision. The bar fill and
//! percentage tail share a utilization-driven color ramp (green →
//! yellow → orange → red). A load-avg footer sits on the last row
//! when the box is tall enough.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

/// Maximum samples retained in the mean-usage history ring buffer.
const HISTORY_CAPACITY: usize = 120;

/// Rows reserved for the total-usage histogram below the per-core bars.
const HISTOGRAM_ROWS: u16 = 4;

/// State holding the most recent CPU snapshot plus a short history of
/// the mean across cores for the footer histogram.
#[derive(Default)]
pub struct CpuState {
    pub per_core: Vec<u8>,
    pub load_avg: Option<(f32, f32, f32)>,
    pub history: Vec<u8>,
}

impl CpuState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace `per_core`/`load_avg` and record the current mean into the
    /// history ring. Called once per `pollStats` snapshot.
    pub fn set_snapshot(&mut self, per_core: Vec<u8>, load_avg: Option<(f32, f32, f32)>) {
        self.per_core = per_core;
        self.load_avg = load_avg;
        if self.history.len() >= HISTORY_CAPACITY {
            self.history.remove(0);
        }
        self.history.push(self.mean());
    }

    /// Mean utilization across all cores, 0..100.
    pub fn mean(&self) -> u8 {
        if self.per_core.is_empty() {
            0
        } else {
            let sum: u32 = self.per_core.iter().map(|&v| u32::from(v)).sum();
            u8::try_from(sum / self.per_core.len() as u32).unwrap_or(0)
        }
    }

    /// Number of rows the CPU box needs given current content: two
    /// border rows + one row per core + one load-avg row + the fixed
    /// histogram strip. Callers cap the box height to this so it
    /// doesn't stretch to fill the terminal.
    pub fn desired_height(&self) -> u16 {
        let cores = u16::try_from(self.per_core.len()).unwrap_or(0);
        let load = u16::from(self.load_avg.is_some());
        let content = cores + load + HISTOGRAM_ROWS;
        content.saturating_add(2).max(5)
    }
}

pub struct CpuWidget<'a> {
    state: &'a CpuState,
}

impl<'a> CpuWidget<'a> {
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

    // One char of breathing room on either side of every row.
    if area.width < 4 {
        return;
    }
    let content = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width - 2,
        height: area.height,
    };

    // Budget rows top-down: per-core bars → load line → histogram strip.
    // Each lower section only gets space if rows remain.
    let histogram_rows = HISTOGRAM_ROWS.min(content.height.saturating_sub(1));
    let load_rows: u16 = u16::from(state.load_avg.is_some() && content.height > histogram_rows + 1);
    let core_rows = content.height.saturating_sub(load_rows + histogram_rows);
    let visible = (core_rows as usize).min(state.per_core.len());

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
    // Layout: "cNN " (4) + bar (fills) + " PPP%" (5)
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

/// Render a horizontal bar filled to `pct` percent into `area`. Fill color
/// matches the percentage tail so each row reads as one visual unit.
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

/// Usage color ramp: green → yellow → orange → red.
fn color_for(pct: u8) -> Color {
    match pct {
        0..=49 => Color::Green,
        50..=69 => Color::Yellow,
        70..=84 => Color::Rgb(255, 140, 0),
        _ => Color::Red,
    }
}
