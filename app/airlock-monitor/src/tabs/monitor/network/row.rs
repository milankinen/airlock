//! Row layout for the network panel lists.
//!
//! Sets the columns, text format and selection highlight of the rows in the
//! Requests and Connections sub-tabs.

use std::time::SystemTime;

use ratatui::style::{Color, Modifier};
use ratatui::text::Line;

/// Width of the leading `⦿` bullet (1 char, no padding).
pub const BULLET_COLS: usize = 1;
/// Width of a timestamp column. The size is for `"Mon DD, HH:MM:SS"`.
pub const TIMESTAMP_COLS: usize = 16;
/// Width of the trailing `Allowed` / `Denied` column.
pub const RESULT_COLS: usize = 7;
/// Width of the `↑ 1.2MB ↓ 340KB` transfer column. The widest realistic
/// pair (six glyphs for each number, plus arrows and spaces) uses 17 cells.
/// The column has 2 more cells.
pub const TRANSFER_COLS: usize = 19;

/// Format a byte count in the short, approximate style of the `curl` and
/// `wget` progress output, for example `6.3GB`, `12MB`, `840KB`, `19B`.
pub fn format_transfer(bytes: u64) -> String {
    // Use powers of 1024, but with the shorter SI-style suffixes. The column
    // is too narrow for `GiB`. Also, with one decimal place, the difference
    // is not important for the column's purpose (how much data moved).
    // Use one decimal only below 10, so that the width stays the same.
    const UNITS: [(u64, &str); 4] = [
        (1024 * 1024 * 1024 * 1024, "TB"),
        (1024 * 1024 * 1024, "GB"),
        (1024 * 1024, "MB"),
        (1024, "KB"),
    ];
    for (scale, suffix) in UNITS {
        if bytes >= scale {
            let value = bytes as f64 / scale as f64;
            return if value < 10.0 {
                format!("{value:.1}{suffix}")
            } else {
                format!("{value:.0}{suffix}")
            };
        }
    }
    format!("{bytes}B")
}

/// Cut `s` to at most `width` chars. If the text is too long, the end is
/// replaced with `…`.
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

/// Cut `s` to at most `width` chars. If the text is too long, the start is
/// replaced with `…`.
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

/// Add spaces after `s` until it is `width` chars wide. Longer text stays
/// the same.
pub fn pad_right(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    let mut out = String::from(s);
    out.extend(std::iter::repeat_n(' ', width - n));
    out
}

/// Add spaces before `s` until it is `width` chars wide. Longer text stays
/// the same.
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

/// Format a `SystemTime` as local time in the format "Mon DD, HH:MM:SS".
pub fn format_timestamp(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let tt = secs as libc::time_t;
    // Convert to local time with `localtime_r` from libc.
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

/// Mark the line as selected with a dark gray background on all spans.
///
/// Also changes the normal (unset) fg to white, and `DarkGray` to a lighter
/// gray. Thus the row is easy to read on the highlight background, and dim
/// text stays different from primary text. Other explicit colors (bullet,
/// green or red status) stay the same.
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

#[cfg(test)]
mod tests {
    //! Tests of the transfer column of the network rows.

    use super::*;

    /// Test that transfer sizes have one decimal only below 10 of a unit. The
    /// column stays short and easy to read.
    ///   1. Format sizes from 0 B to 10 GB
    ///   2. Check the text of each size
    #[test]
    fn format_transfer_uses_one_decimal_only_below_ten() {
        let gb = 1024 * 1024 * 1024;
        for (bytes, text) in [
            (0, "0B"),
            (512, "512B"),
            (1024, "1.0KB"),
            (20 * 1024, "20KB"),
            (6 * 1024 * 1024, "6.0MB"),
            (512 * 1024 * 1024, "512MB"),
            (gb * 63 / 10, "6.3GB"),
            (gb * 99 / 10, "9.9GB"),
            (gb * 10, "10GB"),
        ] {
            assert_eq!(format_transfer(bytes), text, "{bytes}");
        }
    }

    /// Test that the up and down transfer pair fits the column up to 1023 TB,
    /// and that a larger pair is cut to the column width.
    ///   1. Format pairs of the largest value of each unit up to TB
    ///   2. Check that each pair fits the column
    ///   3. Format a pair of the maximum value and check that it is cut to the
    ///      column width
    #[test]
    fn transfer_pair_fits_column_up_to_petabytes_and_is_truncated_beyond() {
        for bytes in [
            1023,
            1024 * 1023,
            1024 * 1024 * 1023,
            1024 * 1024 * 1024 * 1023,
        ] {
            let pair = format!("↑ {} ↓ {}", format_transfer(bytes), format_transfer(bytes));
            assert!(pair.chars().count() <= TRANSFER_COLS, "{pair:?}");
        }
        let huge = format!(
            "↑ {} ↓ {}",
            format_transfer(u64::MAX),
            format_transfer(u64::MAX)
        );
        assert!(huge.chars().count() > TRANSFER_COLS);
        let rendered = pad_right(&truncate_right(&huge, TRANSFER_COLS), TRANSFER_COLS);
        assert_eq!(rendered.chars().count(), TRANSFER_COLS);
    }
}
