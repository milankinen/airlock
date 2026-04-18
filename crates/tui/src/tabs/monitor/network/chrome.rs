//! Outer chrome for the network panel: rounded border with a title + mode
//! indicator, and the sub-tab header row with a separator.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget};

use super::NetworkSubTab;
use crate::Policy;

/// Draw the rounded border + left title + right-aligned "policy: …" anchor.
/// Returns the inner content rect plus the anchor rect (for click detection).
pub fn render_frame(area: Rect, policy: Policy, buf: &mut Buffer) -> (Rect, Rect) {
    let title = Line::from(Span::styled(
        " network ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let label = policy.title();
    // Fixed-width anchor so the label's left edge doesn't dance as the
    // policy changes. " p" — "p" is the keyboard shortcut, lifted to
    // white + bold so it reads as a hint. "olicy:" and " ▾ " fade into
    // the title bar. For shorter labels, the gap before " ▾ " is filled
    // with `─` so the anchor's total width stays constant.
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
    // Cyan matches the `R`/`C` accelerator tint on the sub-tab labels so
    // the keyboard hints read as a unified vocabulary.
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

    // Anchor sits on the top border row, right-aligned with a 1-column gap
    // before the rounded corner.
    let anchor_x = area
        .x
        .saturating_add(area.width)
        .saturating_sub(anchor_width + 1);
    let anchor = Rect::new(anchor_x, area.y, anchor_width, 1);
    (inner, anchor)
}

/// Render the policy dropdown overlay anchored under the title label. Returns
/// the per-row click rects (one entry per `Policy::ALL` variant).
pub fn render_policy_dropdown(
    panel: Rect,
    anchor: Rect,
    highlighted: Policy,
    buf: &mut Buffer,
) -> Vec<(Policy, Rect)> {
    // Width: fit the longest label plus 1 space of padding between the
    // text and each border; cap at panel width.
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
    // reads " policy: <label> ▾ "; `<label>` starts 9 cols into the anchor
    // (1 leading space + "policy: "). Inside the dropdown, the label starts
    // 2 cols into the box (1 border + 1 padding). So `anchor.x + 9 = x + 2`
    // ⇒ `x = anchor.x + 7`. Nudge left if the panel can't hold that.
    let desired_x = anchor.x.saturating_add(7);
    let max_x = panel.x + panel.width.saturating_sub(width);
    let x = desired_x.min(max_x).max(panel.x);
    let y = anchor.y + 1;
    let rect = Rect::new(x, y, width, height);

    Clear.render(rect, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(rect);
    block.render(rect, buf);

    let mut rects = Vec::with_capacity(Policy::ALL.len());
    for (i, policy) in Policy::ALL.iter().enumerate() {
        let row = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
        let active = *policy == highlighted;
        let style = if active {
            Style::default()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
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

/// Rects produced by `render_sub_tabs`, for mouse hit-testing.
pub struct SubTabRects {
    pub requests: Rect,
    pub connections: Rect,
    /// `Some` only when the details tab is currently visible.
    pub details: Option<Rect>,
    /// Click rect for the `×` close glyph at the end of the details label.
    pub details_close: Option<Rect>,
}

/// Render the sub-tab labels with a blank top-margin row and a bottom
/// separator. `details_label` is `Some(text)` (e.g. "Request details") only
/// while the details sub-tab is visible.
pub fn render_sub_tabs(
    area: Rect,
    active: NetworkSubTab,
    details_label: Option<&str>,
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

    let (details_rect, details_close_rect) = if let Some(text) = details_label {
        // Details label + trailing " × ". Shortcut-letter styling doesn't
        // apply (no single-letter accelerator) so render it as a plain word.
        let word_len = text.chars().count() as u16;
        // " " + word + " × " (space-×-space as close button).
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

/// Render the third sub-tab label (details). Active highlighting mirrors
/// `render_label`; the trailing ` × ` is always dim so it reads as a
/// clickable close hint.
fn render_details_label(rect: Rect, text: &str, active: bool, buf: &mut Buffer) {
    let word_style = if active {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::default().fg(Color::Gray)
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
