//! Connections sub-tab — raw TCP connect events.

use std::time::SystemTime;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::row::{
    BULLET_COLS, RESULT_COLS, TIMESTAMP_COLS, apply_row_highlight, format_timestamp, pad_right,
    truncate_right,
};

/// A single connection log entry.
#[derive(Clone)]
pub struct ConnectionEntry {
    pub id: u64,
    pub timestamp: SystemTime,
    pub host: String,
    pub port: u16,
    pub allowed: bool,
    /// Set when a matching `Disconnect` event arrives. `None` means the
    /// connection is still open.
    pub disconnected_at: Option<SystemTime>,
}

impl ConnectionEntry {
    pub fn from_info(info: &crate::ConnectInfo) -> Self {
        Self {
            id: info.id,
            timestamp: info.timestamp,
            host: info.host.clone(),
            port: info.port,
            allowed: info.allowed,
            disconnected_at: None,
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

        // Layout:
        //   "  " + ⦿(1) + "  " + TARGET(expand) + " " + connected(16) +
        //   "  " + disconnected(16) + " " + status(7) + " "
        // (Two spaces after the bullet — the extra breath visually
        // separates the status indicator from the white target text;
        // same reasoning between connected/disconnected.)
        let fixed =
            2 + BULLET_COLS + 2 + 1 + TIMESTAMP_COLS + 2 + TIMESTAMP_COLS + 1 + RESULT_COLS + 1;
        let target_w = (area.width as usize).saturating_sub(fixed);

        let header = {
            let style = Style::default().fg(Color::DarkGray);
            Line::from(vec![
                Span::raw("  "),
                Span::styled(pad_right("", BULLET_COLS), style),
                Span::raw("  "),
                Span::styled(
                    pad_right(&truncate_right("Target", target_w), target_w),
                    style,
                ),
                Span::raw(" "),
                Span::styled(
                    pad_right(
                        &truncate_right("Connected at", TIMESTAMP_COLS),
                        TIMESTAMP_COLS,
                    ),
                    style,
                ),
                Span::raw("  "),
                Span::styled(
                    pad_right(
                        &truncate_right("Disconnected at", TIMESTAMP_COLS),
                        TIMESTAMP_COLS,
                    ),
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
            let mut row = build_connection_row(e, target_w);
            if self.selected == Some(display_idx) {
                apply_row_highlight(&mut row);
            }
            lines.push(row);
        }

        Paragraph::new(lines).render(area, buf);
    }
}

fn build_connection_row(e: &ConnectionEntry, target_w: usize) -> Line<'static> {
    let open = e.disconnected_at.is_none();
    let bullet_color = if e.allowed && open {
        Color::Green
    } else if !e.allowed {
        Color::Red
    } else {
        Color::DarkGray
    };
    let status_text = if e.allowed { "Allowed" } else { "Denied" };
    let status_padded = pad_right(status_text, RESULT_COLS);
    let status_color = if e.allowed { Color::Green } else { Color::Red };

    let connected = format_timestamp(e.timestamp);
    let disconnected = e
        .disconnected_at
        .map_or_else(|| " ".repeat(TIMESTAMP_COLS), format_timestamp);

    let target = format!("{}:{}", e.host, e.port);
    let target = pad_right(&truncate_right(&target, target_w), target_w);

    Line::from(vec![
        Span::raw("  "),
        Span::styled("⦿", Style::default().fg(bullet_color)),
        Span::raw("  "),
        Span::styled(target, Style::default().fg(Color::White)),
        Span::raw(" "),
        Span::styled(connected, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(disconnected, Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(status_padded, Style::default().fg(status_color)),
        Span::raw(" "),
    ])
}
