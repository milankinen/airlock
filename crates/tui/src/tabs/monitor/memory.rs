//! Memory widget — total/used text + used% sparkline histogram.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

/// Maximum samples retained in the used% history ring buffer.
const HISTORY_CAPACITY: usize = 120;

/// State holding the most recent memory snapshot plus a usage ring buffer.
#[derive(Default)]
pub struct MemoryState {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub history: Vec<u8>,
}

impl MemoryState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace total/used and append the latest used% to the history ring.
    pub fn set_usage(&mut self, total_bytes: u64, used_bytes: u64) {
        self.total_bytes = total_bytes;
        self.used_bytes = used_bytes;
        if self.history.len() >= HISTORY_CAPACITY {
            self.history.remove(0);
        }
        self.history.push(self.used_percent());
    }

    /// Used percentage 0..100.
    pub fn used_percent(&self) -> u8 {
        if self.total_bytes == 0 {
            0
        } else {
            ((self.used_bytes * 100) / self.total_bytes).min(100) as u8
        }
    }
}

pub struct MemoryWidget<'a> {
    state: &'a MemoryState,
}

impl<'a> MemoryWidget<'a> {
    pub fn new(state: &'a MemoryState) -> Self {
        Self { state }
    }
}

impl Widget for MemoryWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title = Line::from(Span::styled(
            " memory ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        let right = Line::from(Span::styled(
            format!(" {}% ", self.state.used_percent()),
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

        if inner.height == 0 || inner.width < 10 {
            return;
        }

        render_body(inner, self.state, buf);
    }
}

fn render_body(area: Rect, state: &MemoryState, buf: &mut Buffer) {
    // Two text rows at the top, sparkline fills the rest.
    let text_rows = area.height.min(2);
    let spark_rows = area.height.saturating_sub(text_rows);
    let [text_area, spark_area] = Layout::vertical([
        Constraint::Length(text_rows),
        Constraint::Length(spark_rows),
    ])
    .areas(area);

    if state.total_bytes == 0 {
        Paragraph::new(Line::from(Span::styled(
            "awaiting stats…",
            Style::default().fg(Color::DarkGray),
        )))
        .render(text_area, buf);
        return;
    }

    let lines = vec![
        Line::from(vec![
            Span::styled(" total  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format_bytes(state.total_bytes)),
        ]),
        Line::from(vec![
            Span::styled(" used   ", Style::default().fg(Color::DarkGray)),
            Span::raw(format_bytes(state.used_bytes)),
        ]),
    ];
    Paragraph::new(lines).render(text_area, buf);

    if spark_area.height > 0 && !state.history.is_empty() {
        super::histogram::render(spark_area, &state.history, Color::Blue, buf);
    }
}

/// Format a byte count as `12.3 GiB` / `512 MiB` / `128 KiB` / `96 B`.
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    const TIB: u64 = GIB * 1024;

    if bytes >= TIB {
        format!("{:.1} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.0} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_scales() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }

    #[test]
    fn set_usage_pushes_history_and_caps() {
        let mut s = MemoryState::new();
        for _ in 0..(HISTORY_CAPACITY + 10) {
            s.set_usage(100, 50);
        }
        assert_eq!(s.history.len(), HISTORY_CAPACITY);
        assert!(s.history.iter().all(|&v| v == 50));
    }
}
