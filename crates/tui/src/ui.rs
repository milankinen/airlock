//! Layout and rendering for the TUI.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::app::{App, Tab};
use crate::pty::TuiTerminalSink;
use crate::tabs::network::NetworkWidget;
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

/// Calculate clickable tab header rectangles for mouse handling. Must match
/// the layout produced by `render_tab_bar`.
pub fn tab_header_rects(size: Rect) -> Vec<(Tab, Rect)> {
    let mut rects = Vec::new();
    if size.height == 0 {
        return rects;
    }
    // Tabs live on the bottom row of the terminal.
    let y = size.y + size.height - 1;
    let mut x = size.x + 1; // 1 char left padding

    // "  F1 Sandbox  " = 14 chars
    let sandbox_w = 14;
    rects.push((Tab::Sandbox, Rect::new(x, y, sandbox_w, 1)));
    x += sandbox_w + 1;

    // "  F2 Network (99999)  " — widest plausible label
    let network_w = 22;
    rects.push((Tab::Network, Rect::new(x, y, network_w, 1)));

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
        Tab::Network => {
            NetworkWidget::new(&app.network, &app.policy).render(body, f.buffer_mut());
        }
    }

    // Tab bar at the bottom
    render_tab_bar(f, tab_area, app);
}

fn render_tab_bar(f: &mut Frame<'_>, area: Rect, app: &App) {
    let sandbox_sel = app.active_tab == Tab::Sandbox;
    let network_sel = app.active_tab == Tab::Network;

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
    let hotkey_style = |bg: Color| -> Style { Style::default().fg(Color::Yellow).bg(bg) };

    let sb_bg = tab_bg(sandbox_sel);
    let nw_bg = tab_bg(network_sel);

    let count = app.network.total_count();
    let network_title = if count > 0 {
        format!(" Network ({count})  ")
    } else {
        " Network  ".to_string()
    };

    let spans = vec![
        Span::raw(" "),
        Span::styled("  ", Style::default().bg(sb_bg)),
        Span::styled("F1", hotkey_style(sb_bg)),
        Span::styled(" Sandbox  ", title_style(sandbox_sel, sb_bg)),
        Span::raw(" "),
        Span::styled("  ", Style::default().bg(nw_bg)),
        Span::styled("F2", hotkey_style(nw_bg)),
        Span::styled(network_title, title_style(network_sel, nw_bg)),
    ];

    let line = Line::from(spans);
    // Paint the bg only on the bottom tabs row (height 1); the row above is
    // a blank gap at the terminal's default bg.
    let tabs_row = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
    Paragraph::new(line)
        .style(Style::default().bg(Color::Black))
        .render(tabs_row, f.buffer_mut());
}
