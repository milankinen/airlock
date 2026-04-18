//! Connections sub-tab — raw TCP connect events.

use std::time::SystemTime;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::requests::apply_selection_highlight;
use super::row::{MiddleColumns, build_row};

/// A single connection log entry.
#[derive(Clone)]
pub struct ConnectionEntry {
    pub timestamp: SystemTime,
    pub host: String,
    pub port: u16,
    pub allowed: bool,
}

impl ConnectionEntry {
    pub fn from_info(info: &crate::ConnectInfo) -> Self {
        Self {
            timestamp: info.timestamp,
            host: info.host.clone(),
            port: info.port,
            allowed: info.allowed,
        }
    }
}

pub struct ConnectionsWidget<'a> {
    entries: &'a [ConnectionEntry],
    selected: Option<usize>,
}

impl<'a> ConnectionsWidget<'a> {
    pub fn new(entries: &'a [ConnectionEntry], selected: Option<usize>) -> Self {
        Self { entries, selected }
    }
}

impl Widget for ConnectionsWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        if self.entries.is_empty() {
            let line = Line::from(Span::styled(
                "  No TCP connections observed yet.",
                Style::default().fg(Color::DarkGray),
            ));
            Paragraph::new(line).render(area, buf);
            return;
        }

        let total = self.entries.len();
        let visible = area.height as usize;
        let selected = self.selected.unwrap_or(0);
        let start = selected.saturating_sub(visible.saturating_sub(1));
        let end = (start + visible).min(total);

        let lines: Vec<Line> = (start..end)
            .map(|display_idx| {
                let vec_idx = total - 1 - display_idx;
                let e = &self.entries[vec_idx];
                let target = format!("{}:{}", e.host, e.port);
                let mut row = build_row(
                    e.timestamp,
                    e.allowed,
                    MiddleColumns {
                        left: "",
                        right: &target,
                    },
                    area.width,
                );
                if self.selected == Some(display_idx) {
                    apply_selection_highlight(&mut row);
                }
                row
            })
            .collect();

        Paragraph::new(lines).render(area, buf);
    }
}
