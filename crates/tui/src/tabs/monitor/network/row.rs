//! Shared row-rendering helpers for the network panel. Both Requests and
//! Connections rows share the same bullet + timestamp + target + status
//! skeleton; this module centralizes the styling and truncation logic.

use std::time::SystemTime;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// One column of content shown between the timestamp and the status.
#[derive(Clone, Copy)]
pub struct MiddleColumns<'a> {
    /// Left-aligned text (e.g. `"POST /foo/bar"`) — truncated on the right.
    pub left: &'a str,
    /// Right-aligned text (e.g. `"api.example.com:443"`) — truncated on
    /// the left with an ellipsis.
    pub right: &'a str,
}

/// Fixed width of the status column. `"Allowed"` is 7 chars; `"Denied"`
/// is padded to the same width so the status column aligns between rows.
const STATUS_W: usize = 7;

/// Build a row: bullet + timestamp + MIDDLE + status.
///
/// `total_width` is the inner-panel row width (after the panel border).
pub fn build_row(
    timestamp: SystemTime,
    allowed: bool,
    middle: MiddleColumns<'_>,
    total_width: u16,
) -> Line<'static> {
    let bullet_color = if allowed { Color::Green } else { Color::Red };
    let status_text = if allowed { "Allowed" } else { "Denied" };
    let status_padded = pad_right(status_text, STATUS_W);

    let ts = format_timestamp(timestamp);
    let ts_len = ts.chars().count();

    // Fixed spacing:
    //   "  " + bullet(1) + "  " + ts + "  " + [middle] + "  " + status(7) + " "
    let fixed = 2 + 1 + 2 + ts_len + 2 + 2 + STATUS_W + 1;
    let middle_w = (total_width as usize).saturating_sub(fixed);

    let (left, right) = split_middle(middle.left, middle.right, middle_w);

    Line::from(vec![
        Span::raw("  "),
        Span::styled("⦿", Style::default().fg(bullet_color)),
        Span::raw("  "),
        Span::styled(ts, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(left, Style::default().fg(Color::Gray)),
        Span::styled(right, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(status_padded, Style::default().fg(bullet_color)),
        Span::raw(" "),
    ])
}

/// Split `total` middle columns between `left` (left-aligned,
/// right-truncated with `…`) and `right` (right-aligned, left-truncated
/// with `…`). Returned pair has a combined width of exactly `total`; the
/// left column absorbs any gap as trailing spaces.
fn split_middle(left: &str, right: &str, total: usize) -> (String, String) {
    if total == 0 {
        return (String::new(), String::new());
    }
    // Give `right` up to half the space (clamped to its natural length).
    let right_max = (total / 2).min(right.chars().count());
    let right_rendered = truncate_left(right, right_max);
    let right_len = right_rendered.chars().count();

    // Left gets everything else. Reserve 2 cols of internal gap (absorbed as
    // trailing padding on the left column) when `right` has content.
    let gap = if right_len == 0 { 0 } else { 2 };
    let left_w = total.saturating_sub(right_len);
    let left_content_w = left_w.saturating_sub(gap);
    let left_rendered = truncate_right(left, left_content_w);
    let left_padded = pad_right(&left_rendered, left_w);
    (left_padded, right_rendered)
}

fn truncate_right(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out: String = chars[..width - 1].iter().collect();
    out.push('…');
    out
}

fn truncate_left(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out = String::with_capacity(width);
    out.push('…');
    let tail = &chars[chars.len() - (width - 1)..];
    out.extend(tail);
    out
}

fn pad_right(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    let mut out = String::from(s);
    out.extend(std::iter::repeat_n(' ', width - n));
    out
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Format a `SystemTime` as local "Mon DD, HH:MM:SS" using libc's `localtime_r`.
pub fn format_timestamp(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let tt = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&raw const tt, &raw mut tm).is_null() };
    if !ok {
        return "--- --, --:--:--".to_string();
    }
    let mon = MONTHS
        .get(tm.tm_mon.clamp(0, 11) as usize)
        .copied()
        .unwrap_or("???");
    format!(
        "{} {:02}, {:02}:{:02}:{:02}",
        mon, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec
    )
}
