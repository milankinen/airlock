//! Connections sub-tab of the network panel.
//!
//! Shows the list of raw TCP connections from the sandbox.

use std::time::SystemTime;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::row::{
    BULLET_COLS, RESULT_COLS, TIMESTAMP_COLS, TRANSFER_COLS, apply_row_highlight, format_timestamp,
    format_transfer, pad_right, truncate_right,
};

/// One TCP connection in the connection list.
#[derive(Clone)]
pub struct ConnectionEntry {
    /// Connection ID, from [`crate::ConnectInfo::id`].
    pub id: u64,
    /// Time of the connect attempt.
    pub timestamp: SystemTime,
    /// Target host.
    pub host: String,
    /// Target port.
    pub port: u16,
    /// True if the network policy allowed the connection.
    pub allowed: bool,
    /// Time of the matching `Disconnect` event. `None` means that the
    /// connection is still open.
    pub disconnected_at: Option<SystemTime>,
    /// Total bytes from guest to server.
    pub up: u64,
    /// Total bytes from server to guest.
    pub down: u64,
}

impl ConnectionEntry {
    /// Create an open connection entry with zero traffic from a connect
    /// event.
    pub fn from_info(info: &crate::ConnectInfo) -> Self {
        Self {
            id: info.id,
            timestamp: info.timestamp,
            host: info.host.clone(),
            port: info.port,
            allowed: info.allowed,
            disconnected_at: None,
            up: 0,
            down: 0,
        }
    }
}

/// Minimum number of cells for `host:port`. With less space, the widget
/// removes the transfer column, so that the target is not cut to an
/// ellipsis.
const MIN_TARGET_COLS: usize = 24;

/// Widget that draws the connection list, newest first.
pub struct ConnectionsWidget<'a> {
    entries: &'a [ConnectionEntry],
    selected: Option<usize>,
}

impl<'a> ConnectionsWidget<'a> {
    /// Create a connection list widget.
    /// Args:
    ///  - `entries`: Connection entries, oldest first
    ///  - `selected`: Selected display index (0 = newest), if any.
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
        //   "  " + ⦿(1) + "  " + TARGET(expand) [+ " " + transfer(19)]
        //   + " " + connected(16) + "  " + disconnected(16) + " " +
        //   status(7) + " "
        // (Two spaces after the bullet. The extra space separates the
        // status indicator from the white target text. The same applies
        // between connected and disconnected.)
        let width = area.width as usize;
        let base =
            2 + BULLET_COLS + 2 + 1 + TIMESTAMP_COLS + 2 + TIMESTAMP_COLS + 1 + RESULT_COLS + 1;
        // In a narrow terminal, the widget removes the transfer column first.
        // At 80 columns, the column leaves approximately 11 cells for the
        // target. That is too few even for a short `host:port`. It is better
        // to remove the column than to show each row as an ellipsis.
        let show_transfer = width.saturating_sub(base + 1 + TRANSFER_COLS) >= MIN_TARGET_COLS;
        let fixed = if show_transfer {
            base + 1 + TRANSFER_COLS
        } else {
            base
        };
        let target_w = width.saturating_sub(fixed);

        let header = {
            let style = Style::default().fg(Color::DarkGray);
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(pad_right("", BULLET_COLS), style),
                Span::raw("  "),
                Span::styled(
                    pad_right(&truncate_right("Target", target_w), target_w),
                    style,
                ),
            ];
            if show_transfer {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    pad_right(&truncate_right("Transferred", TRANSFER_COLS), TRANSFER_COLS),
                    style,
                ));
            }
            spans.extend([
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
            ]);
            Line::from(spans)
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
            let mut row = build_connection_row(e, target_w, show_transfer);
            if self.selected == Some(display_idx) {
                apply_row_highlight(&mut row);
            }
            lines.push(row);
        }

        Paragraph::new(lines).render(area, buf);
    }
}

fn build_connection_row(
    e: &ConnectionEntry,
    target_w: usize,
    show_transfer: bool,
) -> Line<'static> {
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

    let mut spans = vec![
        Span::raw("  "),
        Span::styled("⦿", Style::default().fg(bullet_color)),
        Span::raw("  "),
        Span::raw(target),
    ];

    if show_transfer {
        // Draw one padded string, not separate styled spans for arrows and
        // numbers. Thus the column stays aligned for all number widths.
        // Show empty space, not `↑ 0B ↓ 0B`, before data moves. A denied
        // connection never transfers data, and zeros there are only noise.
        let transfer = if e.up == 0 && e.down == 0 {
            " ".repeat(TRANSFER_COLS)
        } else {
            pad_right(
                &truncate_right(
                    &format!("↑ {} ↓ {}", format_transfer(e.up), format_transfer(e.down)),
                    TRANSFER_COLS,
                ),
                TRANSFER_COLS,
            )
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(transfer, Style::default().fg(Color::DarkGray)));
    }

    spans.extend([
        Span::raw(" "),
        Span::styled(connected, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(disconnected, Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(status_padded, Style::default().fg(status_color)),
        Span::raw(" "),
    ]);
    Line::from(spans)
}
