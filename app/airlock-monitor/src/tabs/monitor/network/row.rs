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
/// Width of the `↑ 1.2MB ↓ 340KB` transfer column. Sized for the widest
/// realistic pair — five glyphs per figure plus arrows and spacing.
pub const TRANSFER_COLS: usize = 19;

/// Human-readable byte count in the short, approximate style used by
/// `curl`/`wget` progress output — `6.3GB`, `12MB`, `840KB`, `19B`.
///
/// Powers of 1024, but labelled with the shorter SI-style suffixes: the
/// column is far too narrow for `GiB`, and at one decimal place the
/// distinction is noise for the "how much did this move" question the
/// column answers. One decimal only below 10 so the width stays put.
pub fn format_transfer(bytes: u64) -> String {
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
        .map_or(0, |d| d.as_secs());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_transfer_scales() {
        assert_eq!(format_transfer(0), "0B");
        assert_eq!(format_transfer(512), "512B");
        assert_eq!(format_transfer(1024), "1.0KB");
        assert_eq!(format_transfer(20 * 1024), "20KB");
        assert_eq!(format_transfer(6 * 1024 * 1024), "6.0MB");
        assert_eq!(format_transfer(512 * 1024 * 1024), "512MB");
    }

    /// One decimal below 10, none at or above it — keeps the column from
    /// jittering in width as a transfer grows.
    #[test]
    fn format_transfer_switches_precision_at_ten() {
        let gb = 1024 * 1024 * 1024;
        assert_eq!(format_transfer(gb * 63 / 10), "6.3GB");
        assert_eq!(format_transfer(gb * 99 / 10), "9.9GB");
        assert_eq!(format_transfer(gb * 10), "10GB");
    }

    /// The widest figure below the petabyte mark is a four-digit one like
    /// `1023KB` (just under the next unit), so the widest pair the column
    /// realistically renders is two of those. It has to fit, or the
    /// timestamp columns beside it would shift.
    #[test]
    fn format_transfer_pair_fits_column() {
        for bytes in [
            1023,
            1024 * 1023,
            1024 * 1024 * 1023,
            1024 * 1024 * 1024 * 1023,
        ] {
            let pair = format!("↑ {} ↓ {}", format_transfer(bytes), format_transfer(bytes));
            assert!(
                pair.chars().count() <= TRANSFER_COLS,
                "{pair:?} exceeds {TRANSFER_COLS} cols"
            );
        }
    }

    /// Past a petabyte the figure would overflow the column — unreachable
    /// for a sandbox connection, but the row builder truncates rather than
    /// pushing the neighbouring columns out of alignment.
    #[test]
    fn absurd_transfer_still_holds_column_width() {
        let pair = format!(
            "↑ {} ↓ {}",
            format_transfer(u64::MAX),
            format_transfer(u64::MAX)
        );
        assert!(pair.chars().count() > TRANSFER_COLS);
        let rendered = pad_right(&truncate_right(&pair, TRANSFER_COLS), TRANSFER_COLS);
        assert_eq!(rendered.chars().count(), TRANSFER_COLS);
    }
}
