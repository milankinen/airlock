//! Outer chrome for the network panel: rounded border with a title + mode
//! indicator, and the sub-tab header row with a separator.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

use super::NetworkSubTab;

/// Draw the rounded border + title + right-aligned mode indicator.
/// `mode_label` is display-only in the current design (e.g. `"always-allow"`).
/// Returns the inner content rect.
pub fn render_frame(area: Rect, mode_label: &str, buf: &mut Buffer) -> Rect {
    let title = Line::from(Span::styled(
        " network ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let label = if mode_label.is_empty() {
        "always-allow"
    } else {
        mode_label
    };
    let mode = Line::from(vec![
        Span::styled(format!(" {label} "), Style::default().fg(Color::Yellow)),
        Span::styled("▾ ", Style::default().fg(Color::DarkGray)),
    ])
    .alignment(Alignment::Right);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title)
        .title(mode);
    let inner = block.inner(area);
    block.render(area, buf);
    inner
}

/// Render the sub-tab labels with a blank top-margin row and a bottom
/// separator. Returns the clickable rects for `Requests` and `Connections`.
pub fn render_sub_tabs(area: Rect, active: NetworkSubTab, buf: &mut Buffer) -> (Rect, Rect) {
    if area.height == 0 {
        return (Rect::default(), Rect::default());
    }

    // Layout: top-margin row (blank) | labels row | separator row.
    let labels_y = area.y + area.height.min(2).saturating_sub(1);
    let sep_y = area.y + area.height.saturating_sub(1);

    let left_pad: u16 = 2;
    let gap: u16 = 3;
    let req_label = " Requests ";
    let conn_label = " Connections ";
    let req_w = req_label.chars().count() as u16;
    let conn_w = conn_label.chars().count() as u16;

    let req_rect = Rect::new(area.x + left_pad, labels_y, req_w, 1);
    let conn_rect = Rect::new(area.x + left_pad + req_w + gap, labels_y, conn_w, 1);

    render_label(req_rect, "Requests", active == NetworkSubTab::Requests, buf);
    render_label(
        conn_rect,
        "Connections",
        active == NetworkSubTab::Connections,
        buf,
    );

    if area.height > 2 {
        let sep_row = Rect::new(area.x, sep_y, area.width, 1);
        let sep: String = "─".repeat(area.width as usize);
        Paragraph::new(Line::from(Span::styled(
            sep,
            Style::default().fg(Color::DarkGray),
        )))
        .render(sep_row, buf);
    }

    (req_rect, conn_rect)
}

/// Render one sub-tab label with a leading/trailing space, the shortcut
/// letter tinted cyan, and an underline under the word (but not the
/// surrounding padding spaces) when active.
fn render_label(rect: Rect, text: &str, active: bool, buf: &mut Buffer) {
    // Outer padding: never underlined — just neutral default.
    let pad_style = Style::default();

    let word_style = if active {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::default().fg(Color::Gray)
    };
    let mut shortcut_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    if active {
        shortcut_style = shortcut_style.add_modifier(Modifier::UNDERLINED);
    }

    let first = text.chars().next().map(String::from).unwrap_or_default();
    let rest: String = text.chars().skip(1).collect();

    let line = Line::from(vec![
        Span::styled(" ", pad_style),
        Span::styled(first, shortcut_style),
        Span::styled(rest, word_style),
        Span::styled(" ", pad_style),
    ]);
    Paragraph::new(line).render(rect, buf);
}
