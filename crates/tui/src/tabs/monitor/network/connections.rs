//! Connections sub-tab — raw TCP connect events.

use std::time::SystemTime;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use super::row::{MiddleColumns, build_row};

/// A single connection log entry.
pub struct ConnectionEntry {
    pub timestamp: SystemTime,
    pub host: String,
    pub port: u16,
    pub allowed: bool,
}

impl ConnectionEntry {
    pub fn new(host: String, port: u16, allowed: bool) -> Self {
        Self {
            timestamp: SystemTime::now(),
            host,
            port,
            allowed,
        }
    }
}

pub struct ConnectionsWidget<'a> {
    entries: &'a [ConnectionEntry],
    scroll_offset: usize,
}

impl<'a> ConnectionsWidget<'a> {
    pub fn new(entries: &'a [ConnectionEntry], scroll_offset: usize) -> Self {
        Self {
            entries,
            scroll_offset,
        }
    }
}

impl Widget for ConnectionsWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        let visible = area.height as usize;
        let start = self.scroll_offset.min(self.entries.len());
        let end = (start + visible).min(self.entries.len());

        let lines: Vec<Line> = self.entries[start..end]
            .iter()
            .map(|e| {
                let target = format!("{}:{}", e.host, e.port);
                build_row(
                    e.timestamp,
                    e.allowed,
                    MiddleColumns {
                        left: "",
                        right: &target,
                    },
                    area.width,
                )
            })
            .collect();

        Paragraph::new(lines).render(area, buf);
    }
}
