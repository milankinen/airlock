//! Requests sub-tab of the network panel.
//!
//! Shows the list of HTTP requests from the sandbox.

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

/// One HTTP request in the request list.
///
/// It keeps all headers, so the Details sub-tab can show them without a
/// second subscription.
#[derive(Clone)]
pub struct RequestEntry {
    /// Request ID, from [`crate::RequestInfo::id`].
    pub id: u64,
    /// Time of the request.
    pub timestamp: SystemTime,
    /// HTTP method.
    pub method: String,
    /// Request path.
    pub path: String,
    /// Target host.
    pub host: String,
    /// Target port.
    pub port: u16,
    /// True if the request was allowed. A middleware denial in the
    /// response changes it to false.
    pub allowed: bool,
    /// Request headers as (name, value) pairs.
    pub headers: Vec<(String, String)>,
    /// Response status, after the reply arrives. `None` while the request
    /// is in progress, or always if the connection stopped before a reply.
    pub status: Option<u16>,
    /// Response headers. Empty until the reply arrives.
    pub response_headers: Vec<(String, String)>,
}

impl RequestEntry {
    /// Create an entry without a response from a request event.
    pub fn from_info(info: &crate::RequestInfo) -> Self {
        Self {
            id: info.id,
            timestamp: info.timestamp,
            method: info.method.clone(),
            path: info.path.clone(),
            host: info.host.clone(),
            port: info.port,
            allowed: info.allowed,
            headers: info.headers.clone(),
            status: None,
            response_headers: Vec::new(),
        }
    }

    /// Add the response data to the entry. A middleware denial changes the
    /// entry from allowed to denied.
    pub fn apply_response(&mut self, info: &crate::ResponseInfo) {
        self.status = Some(info.status);
        self.response_headers.clone_from(&info.headers);
        if info.denied {
            self.allowed = false;
        }
    }
}

/// Widget that draws the request list, newest first.
pub struct RequestsWidget<'a> {
    entries: &'a [RequestEntry],
    selected: Option<usize>,
}

impl<'a> RequestsWidget<'a> {
    /// Create a request list widget.
    /// Args:
    ///  - `entries`: Request entries, oldest first
    ///  - `selected`: Selected display index (0 = newest), if any.
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

        // Calculate the target column width first, so that the header and
        // the rows align.
        let target_w = target_column_width(self.entries);

        // Layout (must match `build_request_row`):
        //   "  " + received(16) + "  " + ENDPOINT(expand) + " " +
        //   target(N) + " " + status(7) + " "
        // (Two spaces after `received` give some empty space between the
        // timestamp and the white endpoint column.)
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

/// Width of the Target column: the widest `host:port` of the entries.
///
/// The maximum is 30 columns, so that one long host name cannot take all
/// space from the Endpoint column. The minimum is 12, so that short targets
/// have some empty space.
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
