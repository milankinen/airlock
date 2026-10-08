//! Memory panel of the Monitor tab.
//!
//! Shows the total and used memory, and a history of the used percent.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

/// Maximum number of samples in the used-percent history.
const HISTORY_CAPACITY: usize = 120;

/// State of the memory panel: the latest memory snapshot and a usage history.
#[derive(Default)]
pub struct MemoryState {
    /// Total guest memory in bytes. Zero until the first snapshot.
    pub total_bytes: u64,
    /// Used guest memory in bytes.
    pub used_bytes: u64,
    /// Used-percent samples, oldest first. At most [`HISTORY_CAPACITY`]
    /// samples.
    pub history: Vec<u8>,
}

impl MemoryState {
    /// Create an empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the total and used memory, and add the used percent to the
    /// history. When the history is full, the oldest sample is removed.
    pub fn set_usage(&mut self, total_bytes: u64, used_bytes: u64) {
        self.total_bytes = total_bytes;
        self.used_bytes = used_bytes;
        if self.history.len() >= HISTORY_CAPACITY {
            self.history.remove(0);
        }
        self.history.push(self.used_percent());
    }

    /// Used memory in percent (0..=100). Zero if the total is not known.
    pub fn used_percent(&self) -> u8 {
        (self.used_bytes * 100)
            .checked_div(self.total_bytes)
            .unwrap_or(0)
            .min(100) as u8
    }
}

/// Widget that draws the memory panel.
pub struct MemoryWidget<'a> {
    state: &'a MemoryState,
}

impl<'a> MemoryWidget<'a> {
    /// Create a widget that draws `state`.
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
    // Two text rows at the top. The histogram uses the remaining rows.
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
        super::histogram::render(spark_area, &state.history, Color::Cyan, buf);
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
    //! Tests of the memory panel state.

    use super::*;

    /// Test that the usage history keeps only the newest samples, so that memory
    /// use stays bounded.
    ///   1. Add 10 more samples than the history can hold
    ///   2. Check the length and the first and last samples
    #[test]
    fn usage_history_is_capped_to_newest_samples() {
        let mut s = MemoryState::new();
        for used in 0..(HISTORY_CAPACITY as u64 + 10) {
            s.set_usage(1000, used);
        }
        // The history holds percents of the 1000 byte total. The oldest
        // kept sample is 10 bytes (1%), the last is 129 bytes (12%).
        assert_eq!(s.history.len(), HISTORY_CAPACITY);
        assert_eq!(s.history.first(), Some(&1));
        assert_eq!(s.history.last(), Some(&12));
    }
}
