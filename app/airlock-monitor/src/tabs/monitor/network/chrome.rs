//! Frame parts of the network panel.
//!
//! Draws the panel border with the title and the current network policy, the
//! policy dropdown and the sub-tab header row.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget};

use super::NetworkSubTab;
use crate::Policy;

/// Draw the panel border, the title on the left, and the "policy: …" title
/// on the right.
/// Args:
///  - `area`: Area of the full panel
///  - `policy`: Current network policy
///  - `buf`: Buffer to draw into.
///
/// Returns:
///   The inner content rect, and the rect of the policy title (for click
///   detection).
pub fn render_frame(area: Rect, policy: Policy, buf: &mut Buffer) -> (Rect, Rect) {
    let title = Line::from(Span::styled(
        " network ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let label = policy.title();
    // The policy title has a fixed width, so the left edge of the label does
    // not move when the policy changes. In " p", "p" is the keyboard
    // shortcut. It has a highlight style, so it shows as a hint. "olicy:"
    // and " ▾ " are dim, as the title bar. For shorter labels, `─` fills the
    // gap before " ▾ ", so the total width stays the same.
    let leading = " ";
    let shortcut = "p";
    let rest = "olicy: ";
    let suffix = " ▾ ";
    let max_label_w = Policy::ALL
        .iter()
        .map(|p| p.title().chars().count())
        .max()
        .unwrap_or(0);
    let fill_len = max_label_w.saturating_sub(label.chars().count());
    let fill: String = "─".repeat(fill_len);
    let anchor_width = (leading.chars().count()
        + shortcut.chars().count()
        + rest.chars().count()
        + max_label_w
        + suffix.chars().count()) as u16;
    let dim_style = Style::default().fg(Color::DarkGray);
    // Cyan is the same color as the `R`/`C` shortcut letters on the sub-tab
    // labels. Thus all keyboard hints look the same.
    let shortcut_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default()
        .fg(policy.color())
        .add_modifier(Modifier::BOLD);
    let mode = Line::from(vec![
        Span::raw(leading),
        Span::styled(shortcut, shortcut_style),
        Span::styled(rest, dim_style),
        Span::styled(label, label_style),
        Span::styled(fill, dim_style),
        Span::styled(suffix, dim_style),
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

    // The policy title is on the top border row, aligned to the right. It
    // ends at the column before the rounded corner.
    let anchor_x = area
        .x
        .saturating_add(area.width)
        .saturating_sub(anchor_width + 1);
    let anchor = Rect::new(anchor_x, area.y, anchor_width, 1);
    (inner, anchor)
}

/// Draw the policy dropdown below the policy title.
/// Args:
///  - `panel`: Area of the full panel. The dropdown stays in this area.
///  - `anchor`: Rect of the policy title, from [`render_frame`]
///  - `highlighted`: Policy to highlight
///  - `buf`: Buffer to draw into.
///
/// Returns:
///   The click rect of each row, in [`Policy::ALL`] order. Empty if the
///   panel is too small for the dropdown.
pub fn render_policy_dropdown(
    panel: Rect,
    anchor: Rect,
    highlighted: Policy,
    buf: &mut Buffer,
) -> Vec<(Policy, Rect)> {
    // Width: the longest label, plus 1 space between the text and each
    // border. Not wider than the panel.
    let label_w = Policy::ALL
        .iter()
        .map(|p| p.title().chars().count() as u16)
        .max()
        .unwrap_or(0);
    let width = (label_w + 4).min(panel.width); // "│ label │"
    let height = (Policy::ALL.len() as u16) + 2; // items + top/bottom border
    if width < 6 || height > panel.height {
        return Vec::new();
    }

    // Align the item text with the policy label in the title bar. The title
    // is " policy: <label> ▾ ". `<label>` starts 9 cols into the title
    // (1 space + "policy: "). In the dropdown, the label starts 2 cols into
    // the box (1 border + 1 padding). So `anchor.x + 9 = x + 2`, thus
    // `x = anchor.x + 7`. Move the box left if the panel is too narrow.
    let desired_x = anchor.x.saturating_add(7);
    let max_x = panel.x + panel.width.saturating_sub(width);
    let x = desired_x.min(max_x).max(panel.x);
    let y = anchor.y + 1;
    let rect = Rect::new(x, y, width, height);

    Clear.render(rect, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(rect);
    block.render(rect, buf);

    let mut rects = Vec::with_capacity(Policy::ALL.len());
    for (i, policy) in Policy::ALL.iter().enumerate() {
        let row = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
        let active = *policy == highlighted;
        let style = if active {
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let line = Line::from(Span::styled(
            format!(" {:<w$} ", policy.title(), w = label_w as usize),
            style,
        ));
        Paragraph::new(line).render(row, buf);
        rects.push((*policy, row));
    }
    rects
}

/// Rects from [`render_sub_tabs`], for mouse hit tests.
pub struct SubTabRects {
    /// Rect of the Requests label.
    pub requests: Rect,
    /// Rect of the Connections label.
    pub connections: Rect,
    /// Rect of the details label. `Some` only when the details tab is
    /// visible.
    pub details: Option<Rect>,
    /// Click rect of the `×` close glyph at the end of the details label.
    pub details_close: Option<Rect>,
}

/// Draw the sub-tab labels, with an empty top margin row and a separator
/// row below.
/// Args:
///  - `area`: Area of the sub-tab header
///  - `active`: Active sub-tab
///  - `details_label`: Text of the details label (for example "Request
///    details"). `Some` only when the details sub-tab is visible.
///  - `highlight_requests_letter`: True to highlight the "R" shortcut letter
///  - `highlight_connections_letter`: True to highlight the "C" shortcut
///    letter
///  - `buf`: Buffer to draw into.
///
/// Returns:
///   The label rects, for mouse hit tests.
pub fn render_sub_tabs(
    area: Rect,
    active: NetworkSubTab,
    details_label: Option<&str>,
    highlight_requests_letter: bool,
    highlight_connections_letter: bool,
    buf: &mut Buffer,
) -> SubTabRects {
    if area.height == 0 {
        return SubTabRects {
            requests: Rect::default(),
            connections: Rect::default(),
            details: None,
            details_close: None,
        };
    }

    // Layout: top margin row (empty), labels row, separator row.
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

    render_label(
        req_rect,
        "Requests",
        active == NetworkSubTab::Requests,
        highlight_requests_letter,
        buf,
    );
    render_label(
        conn_rect,
        "Connections",
        active == NetworkSubTab::Connections,
        highlight_connections_letter,
        buf,
    );

    let (details_rect, details_close_rect) = if let Some(text) = details_label {
        // Details label and " × " after it. The label has no shortcut
        // letter, so it shows as a plain word.
        let word_len = text.chars().count() as u16;
        // " " + word + " × " (space, ×, space is the close button).
        let label_w = word_len + 4;
        let close_w: u16 = 3;
        let label_x = area.x + left_pad + req_w + gap + conn_w + gap;
        let label_rect = Rect::new(label_x, labels_y, label_w, 1);
        let close_rect = Rect::new(label_x + label_w - close_w, labels_y, close_w, 1);
        render_details_label(label_rect, text, active == NetworkSubTab::Details, buf);
        (Some(label_rect), Some(close_rect))
    } else {
        (None, None)
    };

    if area.height > 2 {
        let sep_row = Rect::new(area.x, sep_y, area.width, 1);
        let sep: String = "─".repeat(area.width as usize);
        Paragraph::new(Line::from(Span::styled(
            sep,
            Style::default().fg(Color::DarkGray),
        )))
        .render(sep_row, buf);
    }

    SubTabRects {
        requests: req_rect,
        connections: conn_rect,
        details: details_rect,
        details_close: details_close_rect,
    }
}

/// Draw the third sub-tab label (details).
///
/// The active style is the same as in `render_label`. The ` × ` at the end
/// is always dim, so it shows as a close button.
fn render_details_label(rect: Rect, text: &str, active: bool, buf: &mut Buffer) {
    let word_style = if active {
        Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::default()
    };
    let close_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(text.to_string(), word_style),
        Span::styled(" × ", close_style),
    ]);
    Paragraph::new(line).render(rect, buf);
}

/// Draw one sub-tab label with a space before and after the word.
///
/// When active, the word is underlined, but not the spaces. The first letter
/// is cyan only when `highlight_letter` is true. The caller gives `false` if
/// the user bound the action to a different key, so the hint is not wrong.
fn render_label(rect: Rect, text: &str, active: bool, highlight_letter: bool, buf: &mut Buffer) {
    let pad_style = Style::default();
    let word_style = if active {
        Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::default()
    };

    let line = if highlight_letter {
        let mut shortcut_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        if active {
            shortcut_style = shortcut_style.add_modifier(Modifier::UNDERLINED);
        }
        let first = text.chars().next().map(String::from).unwrap_or_default();
        let rest: String = text.chars().skip(1).collect();
        Line::from(vec![
            Span::styled(" ", pad_style),
            Span::styled(first, shortcut_style),
            Span::styled(rest, word_style),
            Span::styled(" ", pad_style),
        ])
    } else {
        Line::from(vec![
            Span::styled(" ", pad_style),
            Span::styled(text.to_string(), word_style),
            Span::styled(" ", pad_style),
        ])
    };
    Paragraph::new(line).render(rect, buf);
}
