//! Layout and rendering for the TUI.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::app::{App, Tab};
use crate::pty::TuiTerminalSink;
use crate::tabs::monitor::MonitorWidget;
use crate::tabs::sandbox::TerminalWidget;

/// Tab bar height: 1 blank gap row + 1 tabs row. Rendered at the bottom.
pub const TAB_BAR_HEIGHT: u16 = 2;

/// Calculate the body area (everything above the bottom tab bar).
pub fn body_area(size: Rect) -> Rect {
    if size.height < TAB_BAR_HEIGHT + 1 {
        return Rect::default();
    }
    Rect::new(size.x, size.y, size.width, size.height - TAB_BAR_HEIGHT)
}

/// One entry in the bottom tab bar. Computed once from the user's
/// keybindings and reused by both `render_tab_bar` and
/// `tab_header_rects` so the click hitboxes always match what's drawn.
struct TabEntry {
    tab: Tab,
    shortcut: String,
    name: &'static str,
}

impl TabEntry {
    /// Total visual width: 2 leading spaces, shortcut, 1 separator,
    /// name, 2 trailing spaces.
    fn width(&self) -> u16 {
        // ASCII-only by construction, so .len() == display width.
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

/// Calculate clickable tab header rectangles for mouse handling. Must match
/// the layout produced by `render_tab_bar`.
pub fn tab_header_rects(size: Rect, app: &App) -> Vec<(Tab, Rect)> {
    let mut rects = Vec::new();
    if size.height == 0 {
        return rects;
    }
    let y = size.y + size.height - 1;
    let mut x = size.x + 1; // 1 char left padding
    for entry in tab_entries(app) {
        let w = entry.width();
        rects.push((entry.tab, Rect::new(x, y, w, 1)));
        x += w + 1;
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

    // Body content
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

    // Tab bar at the bottom
    render_tab_bar(f, tab_area, app);
}

fn render_tab_bar(f: &mut Frame<'_>, area: Rect, app: &App) {
    let sandbox_sel = app.active_tab == Tab::Sandbox;
    let network_sel = app.active_tab == Tab::Monitor;

    // Each tab has its own bg: DarkGray when selected, Black (inherits bar
    // bg) otherwise. The hotkey stays yellow on whichever bg the tab has.
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
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::raw(" "));
        spans.push(Span::styled("  ", Style::default().bg(bg)));
        spans.push(Span::styled(entry.shortcut.clone(), hotkey_style(bg)));
        spans.push(Span::styled(
            format!(" {}  ", entry.name),
            title_style(selected, bg),
        ));
    }

    let line = Line::from(spans);
    // Paint the bg only on the bottom tabs row (height 1); the row above is
    // a blank gap at the terminal's default bg.
    let tabs_row = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
    let bar_style = Style::default().bg(Color::Black);
    Paragraph::new(line)
        .style(bar_style)
        .render(tabs_row, f.buffer_mut());

    // Right-aligned status indicators on the same row.
    let status = build_status_line(app);
    Paragraph::new(status)
        .style(bar_style)
        .alignment(Alignment::Right)
        .render(tabs_row, f.buffer_mut());
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
    if !app.mouse_captured {
        spans.push(Span::styled(
            "Selection mode — Ctrl+C to copy, Esc to exit",
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
