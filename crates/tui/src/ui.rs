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

/// Tab bar height (1 line for tabs + 1 border).
const TAB_BAR_HEIGHT: u16 = 1;
/// Bottom status bar height.
const STATUS_BAR_HEIGHT: u16 = 1;

/// Calculate the body area (everything between tab bar and status bar).
pub fn body_area(size: Rect) -> Rect {
    if size.height < TAB_BAR_HEIGHT + STATUS_BAR_HEIGHT + 1 {
        return Rect::default();
    }
    Rect::new(
        size.x,
        size.y + TAB_BAR_HEIGHT,
        size.width,
        size.height - TAB_BAR_HEIGHT - STATUS_BAR_HEIGHT,
    )
}

/// Calculate clickable tab header rectangles for mouse handling.
pub fn tab_header_rects(size: Rect) -> Vec<(Tab, Rect)> {
    // " Sandbox " = 9 chars, " Network (42) " = ~14 chars
    // We'll just return fixed-width rects in the tab bar row.
    let mut rects = Vec::new();
    let y = size.y;
    let mut x = 1u16; // 1 char padding

    // Sandbox tab
    let w = 9; // " Sandbox "
    rects.push((Tab::Sandbox, Rect::new(x, y, w, 1)));
    x += w + 1;

    // Network tab (wider to accommodate counter pill)
    let w = 18; // " Network (999) "
    rects.push((Tab::Network, Rect::new(x, y, w, 1)));

    rects
}

/// Render the full TUI frame.
pub fn render(f: &mut Frame<'_>, app: &App, sink: &TuiTerminalSink) {
    let size = f.area();
    if size.height < 3 || size.width < 10 {
        return;
    }

    let [tab_area, body, status_area] = Layout::vertical([
        Constraint::Length(TAB_BAR_HEIGHT),
        Constraint::Min(1),
        Constraint::Length(STATUS_BAR_HEIGHT),
    ])
    .areas(size);

    // Tab bar
    render_tab_bar(f, tab_area, app);

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

    // Status bar
    render_status_bar(f, status_area, app);
}

fn render_tab_bar(f: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans = vec![Span::raw(" ")];

    // Sandbox tab
    let sandbox_style = if app.active_tab == Tab::Sandbox {
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    spans.push(Span::styled(" Sandbox ", sandbox_style));
    spans.push(Span::raw(" "));

    // Network tab with counter pill
    let net_style = if app.active_tab == Tab::Network {
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    let count = app.network.total_count();
    let pill = if count > 0 {
        let pill_color = if app.network.denied_count > 0 {
            Color::Red
        } else {
            Color::Green
        };
        vec![
            Span::styled(" Network ", net_style),
            Span::styled(
                format!(" {count} "),
                Style::default()
                    .fg(Color::White)
                    .bg(pill_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]
    } else {
        vec![Span::styled(" Network ", net_style)]
    };
    spans.extend(pill);

    let line = Line::from(spans);
    Paragraph::new(line)
        .style(Style::default().bg(Color::Black))
        .render(area, f.buffer_mut());
}

fn render_status_bar(f: &mut Frame<'_>, area: Rect, app: &App) {
    let mouse_hint = if app.mouse_captured {
        " Select"
    } else {
        " Capture"
    };
    let line = Line::from(vec![
        Span::styled(" F1", Style::default().fg(Color::Yellow)),
        Span::raw(" Sandbox  "),
        Span::styled("F2", Style::default().fg(Color::Yellow)),
        Span::raw(" Network  "),
        Span::styled("F12", Style::default().fg(Color::Yellow)),
        Span::raw(mouse_hint),
        Span::raw("  "),
        Span::styled("Ctrl+Q", Style::default().fg(Color::Yellow)),
        Span::raw(" Quit"),
    ]);

    Paragraph::new(line)
        .style(Style::default().bg(Color::DarkGray).fg(Color::White))
        .render(area, f.buffer_mut());
}
