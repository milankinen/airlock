//! Shared row-rendering helpers for the network panel. Both Requests and
//! Connections rows share the same column vocabulary (timestamp widths,
//! status widths, truncation, selection highlight); this module hosts
//! those utilities.

use std::time::SystemTime;

use ratatui::style::{Color, Modifier};
use ratatui::text::Line;

/// Width of the leading `⦿` bullet (1 char, no padding).
pub const BULLET_COLS: usize = 1;
/// Width of a fixed-width timestamp column, sized for `"Mon DD, HH:MM:SS"`.
pub const TIMESTAMP_COLS: usize = 16;
/// Width of the trailing `Allowed` / `Denied` column.
pub const RESULT_COLS: usize = 7;

pub fn truncate_right(s: &str, width: usize) -> String {
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

pub fn truncate_left(s: &str, width: usize) -> String {
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

pub fn pad_right(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    let mut out = String::from(s);
    out.extend(std::iter::repeat_n(' ', width - n));
    out
}

pub fn pad_left(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    let mut out = String::with_capacity(width);
    out.extend(std::iter::repeat_n(' ', width - n));
    out.push_str(s);
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

/// Paint every span on the line with a dark-gray background to mark it as
/// selected. Also promotes normal (unset) fg to white and `DarkGray` to a
/// slightly lighter gray so the row reads clearly against the highlight
/// background without losing the dimmed/primary distinction. Other explicit
/// colors (bullet, status green/red) are preserved.
pub fn apply_row_highlight(line: &mut Line<'_>) {
    for span in &mut line.spans {
        let fg = match span.style.fg {
            None | Some(Color::Reset) => Color::White,
            Some(Color::DarkGray) => Color::Rgb(160, 160, 160),
            Some(other) => other,
        };
        span.style = span
            .style
            .bg(Color::Rgb(50, 50, 50))
            .fg(fg)
            .add_modifier(Modifier::BOLD);
    }
}
