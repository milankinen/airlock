//! Requests sub-tab — HTTP request log.

use std::time::SystemTime;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::row::{
    RESULT_COLS, TIMESTAMP_COLS, apply_row_highlight, format_timestamp, pad_left, pad_right,
    truncate_left, truncate_right,
};

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

        // Target column width — picked up front so header and rows align.
        let target_w = target_column_width(self.entries);

        // Layout (must match `build_request_row`):
        //   "  " + received(16) + "  " + ENDPOINT(expand) + " " +
        //   target(N) + " " + status(7) + " "
        // (Two spaces after `received` give the timestamp a bit of
        // breathing room before the white endpoint column.)
        let fixed = 2 + TIMESTAMP_COLS + 2 + 1 + target_w + 1 + RESULT_COLS + 1;
        let endpoint_w = (area.width as usize).saturating_sub(fixed);

        let header = {
            let style = Style::default().fg(Color::DarkGray);
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    pad_right(
                        &truncate_right("Received at", TIMESTAMP_COLS),
                        TIMESTAMP_COLS,
                    ),
                    style,
                ),
                Span::raw("  "),
                Span::styled(
                    pad_right(&truncate_right("Endpoint", endpoint_w), endpoint_w),
                    style,
                ),
                Span::raw(" "),
                Span::styled(
                    pad_left(&truncate_right("Target", target_w), target_w),
                    style,
                ),
                Span::raw(" "),
                Span::styled(
                    pad_right(&truncate_right("Result", RESULT_COLS), RESULT_COLS),
                    style,
                ),
            ])
        };

        let body_height = area.height.saturating_sub(1) as usize;
        if body_height == 0 {
            Paragraph::new(header).render(area, buf);
            return;
        }

        let total = self.entries.len();
        let selected = self.selected.unwrap_or(0);
        let start = selected.saturating_sub(body_height.saturating_sub(1));
        let end = (start + body_height).min(total);

        let mut lines = Vec::with_capacity(1 + end - start);
        lines.push(header);
        for display_idx in start..end {
            let vec_idx = total - 1 - display_idx;
            let e = &self.entries[vec_idx];
            let mut row = build_request_row(e, target_w, endpoint_w);
            if self.selected == Some(display_idx) {
                apply_row_highlight(&mut row);
            }
            lines.push(row);
        }

        Paragraph::new(lines).render(area, buf);
    }
}

/// Cap the Target column at the widest current entry's `host:port`, bounded
/// at 30 columns so a single long host name can't starve the Endpoint
/// column. At least 12 so short targets still render with breathing room.
fn target_column_width(entries: &[RequestEntry]) -> usize {
    let max = entries
        .iter()
        .map(|e| format!("{}:{}", e.host, e.port).chars().count())
        .max()
        .unwrap_or(12);
    max.clamp(12, 30)
}

fn build_request_row(e: &RequestEntry, target_w: usize, endpoint_w: usize) -> Line<'static> {
    let status_text = if e.allowed { "Allowed" } else { "Denied" };
    let status_padded = pad_right(status_text, RESULT_COLS);
    let status_color = if e.allowed { Color::Green } else { Color::Red };

    let received = format_timestamp(e.timestamp);
    let target = format!("{}:{}", e.host, e.port);
    let target = pad_left(&truncate_left(&target, target_w), target_w);

    let endpoint = format!("{} {}", e.method, e.path);
    let endpoint = pad_right(&truncate_right(&endpoint, endpoint_w), endpoint_w);

    Line::from(vec![
        Span::raw("  "),
        Span::styled(received, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::raw(endpoint),
        Span::raw(" "),
        Span::styled(target, Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(status_padded, Style::default().fg(status_color)),
        Span::raw(" "),
    ])
}
