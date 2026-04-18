//! Requests sub-tab — HTTP request log.

use std::time::SystemTime;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::row::{MiddleColumns, build_row};

/// A single HTTP request log entry. Retains the full header set so the
/// Details sub-tab can show them without a second subscribe.
#[derive(Clone)]
pub struct RequestEntry {
    pub timestamp: SystemTime,
    pub method: String,
    pub path: String,
    pub host: String,
    pub port: u16,
    pub allowed: bool,
    pub headers: Vec<(String, String)>,
}

impl RequestEntry {
    pub fn from_info(info: &crate::RequestInfo) -> Self {
        Self {
            timestamp: info.timestamp,
            method: info.method.clone(),
            path: info.path.clone(),
            host: info.host.clone(),
            port: info.port,
            allowed: info.allowed,
            headers: info.headers.clone(),
        }
    }
}

pub struct RequestsWidget<'a> {
    entries: &'a [RequestEntry],
    selected: Option<usize>,
}

impl<'a> RequestsWidget<'a> {
    pub fn new(entries: &'a [RequestEntry], selected: Option<usize>) -> Self {
        Self { entries, selected }
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

        // Display newest first. Scroll so the selected row stays visible:
        // `sel` is pinned to the bottom once it moves past the visible area.
        let total = self.entries.len();
        let visible = area.height as usize;
        let selected = self.selected.unwrap_or(0);
        let start = selected.saturating_sub(visible.saturating_sub(1));
        let end = (start + visible).min(total);

        let lines: Vec<Line> = (start..end)
            .map(|display_idx| {
                // display index 0 = newest = entries[last]
                let vec_idx = total - 1 - display_idx;
                let e = &self.entries[vec_idx];
                let left = format!("{} {}", e.method, e.path);
                let right = format!("{}:{}", e.host, e.port);
                let mut row = build_row(
                    e.timestamp,
                    e.allowed,
                    MiddleColumns {
                        left: &left,
                        right: &right,
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

/// Paint every span on the line with a dark-gray background to mark it as
/// selected. Preserves each span's existing fg so bullet colors and status
/// text stay readable.
pub(super) fn apply_selection_highlight(line: &mut Line<'_>) {
    for span in &mut line.spans {
        let mut style = span.style;
        style = style
            .bg(Color::Rgb(50, 50, 50))
            .add_modifier(Modifier::BOLD);
        span.style = style;
    }
}
