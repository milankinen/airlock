//! Requests sub-tab — HTTP request log.
//!
//! Phase 3 scaffolds this sub-tab; HTTP request events are emitted by the
//! middleware in Phase 4.

use std::time::SystemTime;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::row::{MiddleColumns, build_row};

/// A single HTTP request log entry.
pub struct RequestEntry {
    pub timestamp: SystemTime,
    pub method: String,
    pub path: String,
    pub host: String,
    pub port: u16,
    pub allowed: bool,
}

impl RequestEntry {
    #[allow(dead_code)] // used by Phase 4 network middleware wiring
    pub fn new(method: String, path: String, host: String, port: u16, allowed: bool) -> Self {
        Self {
            timestamp: SystemTime::now(),
            method,
            path,
            host,
            port,
            allowed,
        }
    }
}

pub struct RequestsWidget<'a> {
    entries: &'a [RequestEntry],
    scroll_offset: usize,
}

impl<'a> RequestsWidget<'a> {
    pub fn new(entries: &'a [RequestEntry], scroll_offset: usize) -> Self {
        Self {
            entries,
            scroll_offset,
        }
    }
}

impl Widget for RequestsWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        if self.entries.is_empty() {
            let line = Line::from(Span::styled(
                "  No HTTP requests observed yet.",
                Style::default().fg(Color::DarkGray),
            ));
            Paragraph::new(line).render(area, buf);
            return;
        }

        let visible = area.height as usize;
        let start = self.scroll_offset.min(self.entries.len());
        let end = (start + visible).min(self.entries.len());

        let lines: Vec<Line> = self.entries[start..end]
            .iter()
            .map(|e| {
                let left = format!("{} {}", e.method, e.path);
                let right = format!("{}:{}", e.host, e.port);
                build_row(
                    e.timestamp,
                    e.allowed,
                    MiddleColumns {
                        left: &left,
                        right: &right,
                    },
                    area.width,
                )
            })
            .collect();

        Paragraph::new(lines).render(area, buf);
    }
}
