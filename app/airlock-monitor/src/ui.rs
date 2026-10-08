//! Layout and drawing of the TUI.
//!
//! Draws the full TUI screen. The event handlers also use the layout to resize
//! the sandbox terminal and to find the target of mouse clicks.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::app::{App, Tab};
use crate::pty::TuiTerminalSink;
use crate::tabs::monitor::MonitorWidget;
use crate::tabs::sandbox::TerminalWidget;

/// Height of the tab bar at the bottom: 1 empty row and 1 row of tabs.
pub const TAB_BAR_HEIGHT: u16 = 2;

/// Body area of the TUI: all of `size` above the bottom tab bar.
/// Returns an empty rect if the terminal is too small for a body.
pub fn body_area(size: Rect) -> Rect {
    if size.height < TAB_BAR_HEIGHT + 1 {
        return Rect::default();
    }
    Rect::new(size.x, size.y, size.width, size.height - TAB_BAR_HEIGHT)
}

/// Empty columns before the first tab.
const TAB_BAR_PAD: u16 = 1;
/// Empty columns between two tabs.
const TAB_GAP: u16 = 2;

/// One entry in the bottom tab bar, made from the user's key bindings.
///
/// Both `render_tab_bar` and `tab_header_rects` use these entries. Thus
/// the click areas are always the same as the drawn tabs.
struct TabEntry {
    tab: Tab,
    shortcut: String,
    name: &'static str,
}

impl TabEntry {
    /// Total width on the screen: 2 spaces, shortcut, 1 separator, name and
    /// 2 spaces.
    fn width(&self) -> u16 {
        // The text is always ASCII, so .len() is the display width.
        (2 + self.shortcut.len() + 1 + self.name.len() + 2) as u16
    }
}

fn tab_entries(app: &App) -> [TabEntry; 2] {
    use crate::keys::{Action, format_key};
    let shortcut = |a: Action, fallback: &str| -> String {
        app.settings
            .keys
            .primary(a)
            .map_or_else(|| fallback.to_string(), format_key)
    };
    [
        TabEntry {
            tab: Tab::Sandbox,
            shortcut: shortcut(Action::SwitchSandbox, "F1"),
            name: "Sandbox",
        },
        TabEntry {
            tab: Tab::Monitor,
            shortcut: shortcut(Action::SwitchMonitor, "F2"),
            name: "Monitor",
        },
    ]
}

/// Clickable rectangles of the tab headers, for mouse handling.
/// Args:
///  - `size`: Full terminal area
///  - `app`: Application state (for the key binding labels).
///
/// Returns:
///   Each tab with its header rectangle. The layout must be the same as
///   the layout of `render_tab_bar`.
pub fn tab_header_rects(size: Rect, app: &App) -> Vec<(Tab, Rect)> {
    let mut rects = Vec::new();
    if size.height == 0 {
        return rects;
    }
    let y = size.y + size.height - 1;
    let mut x = size.x + TAB_BAR_PAD;
    for entry in tab_entries(app) {
        let w = entry.width();
        rects.push((entry.tab, Rect::new(x, y, w, 1)));
        x += w + TAB_GAP;
    }
    rects
}

/// Render the full TUI frame.
pub fn render(f: &mut Frame<'_>, app: &App, sink: &TuiTerminalSink) {
    let size = f.area();
    if size.height < 3 || size.width < 10 {
        return;
    }

    let [body, tab_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(TAB_BAR_HEIGHT)]).areas(size);

    match app.active_tab {
        Tab::Sandbox => {
            TerminalWidget::new(sink).render(body, f.buffer_mut());
            if let Some(pos) = crate::tabs::sandbox::cursor_position(sink, body) {
                f.set_cursor_position(pos);
            }
        }
        Tab::Monitor => {
            MonitorWidget::new(&app.monitor, app.network.policy(), &app.settings.keys)
                .render(body, f.buffer_mut());
        }
    }

    // Tab bar at the bottom.
    render_tab_bar(f, tab_area, app);
}

fn render_tab_bar(f: &mut Frame<'_>, area: Rect, app: &App) {
    let sandbox_sel = app.active_tab == Tab::Sandbox;
    let network_sel = app.active_tab == Tab::Monitor;

    // Each tab has its own bg: DarkGray when selected, else Black (the same
    // as the bar bg). The hotkey color is the same on both bgs.
    let tab_bg = |selected: bool| -> Color {
        if selected {
            Color::DarkGray
        } else {
            Color::Black
        }
    };
    let title_style = |selected: bool, bg: Color| -> Style {
        let mut s = Style::default().bg(bg);
        if selected {
            s = s.fg(Color::White).add_modifier(Modifier::BOLD);
        } else {
            s = s.fg(Color::Gray);
        }
        s
    };
    let hotkey_style = |bg: Color| -> Style { Style::default().fg(Color::Cyan).bg(bg) };

    let entries = tab_entries(app);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(1 + entries.len() * 4);
    for (i, entry) in entries.iter().enumerate() {
        let selected = match entry.tab {
            Tab::Sandbox => sandbox_sel,
            Tab::Monitor => network_sel,
        };
        let bg = tab_bg(selected);
        let gap = if i == 0 { TAB_BAR_PAD } else { TAB_GAP };
        spans.push(Span::raw(" ".repeat(gap.into())));
        spans.push(Span::styled("  ", Style::default().bg(bg)));
        spans.push(Span::styled(entry.shortcut.clone(), hotkey_style(bg)));
        spans.push(Span::styled(
            format!(" {}  ", entry.name),
            title_style(selected, bg),
        ));
    }

    let line = Line::from(spans);
    // Paint the bg only on the bottom row of tabs (height 1). The row above
    // is an empty gap with the default bg of the terminal.
    let tabs_row = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
    let bar_style = Style::default().bg(Color::Black);
    Paragraph::new(line)
        .style(bar_style)
        .render(tabs_row, f.buffer_mut());

    // Status indicators on the same row, aligned to the right.
    let status = build_status_line(app);
    Paragraph::new(status)
        .style(bar_style)
        .alignment(Alignment::Right)
        .render(tabs_row, f.buffer_mut());
}

/// Time that the text selection hint stays visible after a click.
const SELECT_HINT_FOR: Duration = Duration::from_secs(2);

/// True if the selection hint must be visible at `now`.
/// Args:
///  - `clicked_at`: Time of the last left click, if there was one
///  - `now`: Current time.
///
/// It is a separate function so that tests can check the time window
/// without a terminal. Also, the status line and its test then always use
/// the same meaning of "visible".
pub(crate) fn select_hint_visible(clicked_at: Option<Instant>, now: Instant) -> bool {
    clicked_at.is_some_and(|t| now.duration_since(t) < SELECT_HINT_FOR)
}

fn build_status_line(app: &App) -> Line<'static> {
    let label = Style::default().fg(Color::Gray);
    let value = Style::default().fg(Color::DarkGray);
    let sep = Span::styled(" │ ", value);

    let cpu_pct = app.monitor.cpu.mean();
    let mem_used = format_bytes(app.monitor.memory.used_bytes);
    let mem_total = format_bytes(app.monitor.memory.total_bytes);
    let allowed = app.monitor.network.request_allowed;
    let denied = app.monitor.network.request_denied;

    let mut spans = Vec::with_capacity(16);
    // Mouse capture stays on for the whole session, so a plain drag never
    // selects text. After a click, show the modifier key that does.
    if select_hint_visible(app.select_hint_at, Instant::now()) {
        spans.push(Span::styled(
            format!("Hold {} to select text", crate::terminal::select_modifier()),
            Style::default().fg(Color::Yellow),
        ));
        spans.push(sep.clone());
    }
    spans.extend([
        Span::styled("CPU ", label),
        Span::styled(format!("{cpu_pct}%"), value),
        sep.clone(),
        Span::styled("Memory ", label),
        Span::styled(format!("{mem_used} / {mem_total}"), value),
        sep,
        Span::styled("Network ", label),
        Span::styled(format!("{allowed}"), Style::default().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(format!("{denied}"), Style::default().fg(Color::Red)),
        Span::raw(" "),
    ]);
    Line::from(spans)
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    const TIB: u64 = GIB * 1024;

    if bytes >= TIB {
        format!("{:.1} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.0} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the status line logic.

    use super::*;

    /// Test that the text selection hint shows for two seconds after a click.
    ///   1. Check that the hint is hidden without a click
    ///   2. Check that it shows at the click and 1.9 seconds later
    ///   3. Check that it is hidden 2.1 seconds after the click
    #[test]
    fn select_hint_after_click_expires_after_two_seconds() {
        let now = Instant::now();
        assert!(!select_hint_visible(None, now));
        assert!(select_hint_visible(Some(now), now));
        assert!(select_hint_visible(
            Some(now),
            now + Duration::from_millis(1_900)
        ));
        assert!(!select_hint_visible(
            Some(now),
            now + Duration::from_millis(2_100)
        ));
    }
}
