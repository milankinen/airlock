//! Drawing of the approval dialog.
//!
//! Draws the dialog in the middle of the screen, on top of all other
//! content. The dialog fits its content, but it is never larger than most
//! of the screen. Content that does not fit scrolls in both directions, and
//! scroll bars show the position. The buttons always stay visible. Plain
//! text wraps to the dialog width. Code, data and diff lines do not wrap.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    StatefulWidget, Widget,
};

use super::{ApprovalDiffKind, ApprovalRequest, Approvals, Button, DialogLayout};

/// Columns between two buttons.
const BUTTON_GAP: u16 = 3;
/// Empty columns between the border and the content, on each side.
const PAD_X: u16 = 2;
/// Columns around the content: 1 border and the padding on each side.
const FRAME_W: u16 = 2 + 2 * PAD_X;
/// Empty rows above the content.
const PAD_TOP: u16 = 1;
/// Empty rows between the content and the buttons.
const GAP_ROWS: u16 = 2;
/// Empty rows below the buttons.
const PAD_BOTTOM: u16 = 1;
/// Rows around the content: 2 borders, the padding, the gap and 1 row of
/// buttons.
const FRAME_H: u16 = 2 + PAD_TOP + GAP_ROWS + 1 + PAD_BOTTOM;
/// Indent of the values under their labels.
const INDENT: &str = "  ";
/// Note in place of the content when the content does not fit.
const TOO_SMALL: &str = "Terminal too small";

/// Shortcut letter and the remaining text of each button. The shortcut
/// is the first letter of the label.
fn button_label(button: Button, expanded: bool) -> (&'static str, &'static str) {
    match button {
        Button::Accept => ("a", "ccept"),
        Button::Deny => ("d", "eny"),
        Button::Toggle if expanded => ("s", "how less"),
        Button::Toggle => ("s", "how more"),
    }
}

/// Screen width of a button.
fn button_width(button: Button, expanded: bool) -> u16 {
    let (key, rest) = button_label(button, expanded);
    (key.len() + rest.len()) as u16
}

/// Rows of each part inside the border, from top to bottom.
struct Rows {
    pad_top: u16,
    content: u16,
    gap: u16,
}

/// Split `height` rows inside the border into the parts of the dialog.
/// The buttons get a row first, then 1 content row, then the padding. All
/// rows that remain go to the content. Thus the padding goes away first
/// on a small screen.
fn split_rows(height: u16) -> Rows {
    let mut left = height.saturating_sub(1);
    let mut take = |want: u16| {
        let got = want.min(left);
        left -= got;
        got
    };
    let content = take(1);
    let gap = take(1);
    let pad_top = take(PAD_TOP);
    let gap = gap + take(GAP_ROWS - 1);
    let _pad_bottom = take(PAD_BOTTOM);
    let content = content + take(u16::MAX);
    Rows {
        pad_top,
        content,
        gap,
    }
}

/// Width of a row with `buttons`.
fn buttons_width(buttons: &[Button], expanded: bool) -> u16 {
    let gaps = buttons.len().saturating_sub(1) as u16 * BUTTON_GAP;
    buttons
        .iter()
        .map(|b| button_width(*b, expanded))
        .sum::<u16>()
        + gaps
}

/// All buttons, in the order on the screen.
const ALL_BUTTONS: [Button; 3] = [Button::Accept, Button::Deny, Button::Toggle];

/// Draw the dialog of the first request in the queue. No effect when the
/// queue is empty.
/// Args:
///  - `approvals`: Queue and view state. It gets the layout of this draw
///    for the input handlers.
///  - `area`: Full screen area
///  - `buf`: Buffer to draw into.
pub(crate) fn render_dialog(approvals: &Approvals, area: Rect, buf: &mut Buffer) {
    let Some(request) = approvals.active() else {
        return;
    };
    let expanded = approvals.expanded();
    // The short form and the expanded form have the same minimum width, so
    // the dialog does not get narrower when the user shows the details.
    let min_w = buttons_width(&ALL_BUTTONS, true) + FRAME_W;
    let max_w = (area.width * 9 / 10).max(min_w).min(area.width);
    let max_h = (area.height * 9 / 10).max(FRAME_H + 1).min(area.height);
    // The frame needs 2 rows and 2 columns for the border, and 1 row and 2
    // columns inside it for a button.
    if max_w < 4 || max_h < 3 {
        render_deny_only(approvals, area, buf);
        return;
    }

    let lines = content(
        request,
        expanded,
        usize::from(max_w.saturating_sub(FRAME_W)),
    );
    let content_w = lines.iter().map(Line::width).max().unwrap_or(0);
    // The scroll offset is 16 bits. Columns after that limit never show,
    // but a line that long is not useful on a terminal.
    let content_w = u16::try_from(content_w).unwrap_or(u16::MAX);
    let content_h = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let width = content_w
        .saturating_add(FRAME_W)
        .clamp(min_w.min(max_w), max_w);
    let height = content_h.saturating_add(FRAME_H).min(max_h);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );

    let inner = rect.inner(ratatui::layout::Margin::new(1, 1));
    let rows = split_rows(inner.height);
    // The padding at the sides also goes away first on a narrow screen.
    let pad_x = PAD_X.min(inner.width.saturating_sub(1) / 2);
    let view = Rect::new(
        inner.x + pad_x,
        inner.y + rows.pad_top,
        inner.width.saturating_sub(2 * pad_x),
        rows.content,
    );

    Clear.render(rect, buf);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));
    // With no content row, the user cannot see the request. Then show a
    // note on the border, and permit only deny.
    if view.height == 0 {
        block = block.title(Line::styled(
            format!(" {TOO_SMALL} "),
            Style::default().fg(Color::Yellow),
        ));
    }
    block.render(rect, buf);
    if inner.height == 0 || inner.width == 0 {
        approvals.set_layout(DialogLayout::default());
        return;
    }

    let max_y = content_h.saturating_sub(view.height);
    let max_x = content_w.saturating_sub(view.width);
    let (scroll_y, scroll_x) = approvals.scroll();
    let (scroll_y, scroll_x) = (scroll_y.min(max_y), scroll_x.min(max_x));
    if view.height > 0 {
        Paragraph::new(lines)
            .scroll((scroll_y, scroll_x))
            .render(view, buf);
    }

    let bar_style = Style::default().fg(Color::Yellow);
    if max_y > 0 {
        // The vertical bar is on the right border, next to the content.
        let track = Rect::new(rect.right() - 1, view.y, 1, view.height);
        let mut state = ScrollbarState::new(usize::from(max_y) + 1)
            .position(usize::from(scroll_y))
            .viewport_content_length(usize::from(view.height));
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(bar_style)
            .render(track, buf, &mut state);
    }
    if max_x > 0 && rows.gap > 0 {
        // The horizontal bar is on the first empty row below the content.
        let track = Rect::new(view.x, view.bottom(), view.width, 1);
        let mut state = ScrollbarState::new(usize::from(max_x) + 1)
            .position(usize::from(scroll_x))
            .viewport_content_length(usize::from(view.width));
        Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
            .style(bar_style)
            .render(track, buf, &mut state);
    }

    let buttons: &[Button] = if view.height > 0 {
        &ALL_BUTTONS
    } else {
        &[Button::Deny]
    };
    let row = Rect::new(inner.x, view.bottom() + rows.gap, inner.width, 1);
    let [accept, deny, toggle] = render_buttons(row, buttons, expanded, buf);
    approvals.set_layout(DialogLayout {
        max_y,
        max_x,
        page: view.height,
        accept,
        deny,
        toggle,
        content: view,
    });
}

/// Draw only the deny button, on a screen with no room for the frame.
/// The user can then still close the dialog.
fn render_deny_only(approvals: &Approvals, area: Rect, buf: &mut Buffer) {
    let mut layout = DialogLayout::default();
    if !area.is_empty() {
        Clear.render(area, buf);
        let row = Rect::new(area.x, area.y + area.height / 2, area.width, 1);
        let [_, deny, _] = render_buttons(row, &[Button::Deny], false, buf);
        layout.deny = deny;
    }
    approvals.set_layout(layout);
}

/// Draw `buttons` in the middle of `row`.
/// Returns:
///   Click rects of the accept, deny and toggle buttons. A rect is empty
///   if the button does not show, and clipped if the row is too narrow.
fn render_buttons(row: Rect, buttons: &[Button], expanded: bool, buf: &mut Buffer) -> [Rect; 3] {
    let key_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let total = buttons_width(buttons, expanded);
    let start = row.x + row.width.saturating_sub(total) / 2;
    let mut x = start;
    let mut spans = Vec::new();
    let mut rects = [Rect::default(); 3];
    for (i, &button) in buttons.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" ".repeat(usize::from(BUTTON_GAP))));
            x = x.saturating_add(BUTTON_GAP);
        }
        let (key, rest) = button_label(button, expanded);
        spans.extend([Span::styled(key, key_style), Span::raw(rest)]);
        let w = button_width(button, expanded);
        let slot = ALL_BUTTONS.iter().position(|b| *b == button).unwrap_or(0);
        rects[slot] = Rect::new(x, row.y, w, 1).intersection(row);
        x = x.saturating_add(w);
    }
    let line_area = Rect::new(start, row.y, row.right().saturating_sub(start), 1);
    Paragraph::new(Line::from(spans)).render(line_area, buf);
    rects
}

/// Lines of the dialog content.
/// Args:
///  - `request`: Request to show
///  - `expanded`: True to show the details in place of the body
///  - `wrap_w`: Width for the plain text. Code, data and diff lines can be
///    wider.
fn content(request: &ApprovalRequest, expanded: bool, wrap_w: usize) -> Vec<Line<'static>> {
    let title = Style::default().add_modifier(Modifier::BOLD);
    let label = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let mono = Style::default().fg(Color::LightBlue);

    let mut lines: Vec<Line<'static>> = wrap(&request.title, wrap_w)
        .into_iter()
        .map(|l| Line::styled(l, title))
        .collect();
    // The short form is the title and the body. The expanded form is the
    // title and the details, with no body. The body can be a click hint
    // that has no use in the expanded form.
    if !expanded {
        if !request.body.is_empty() {
            lines.push(Line::default());
            lines.extend(wrap(&request.body, wrap_w).into_iter().map(Line::raw));
        }
        return lines;
    }

    let value_w = wrap_w.saturating_sub(INDENT.len()).max(1);
    for detail in &request.details {
        lines.push(Line::default());
        lines.extend(
            wrap(&detail.label, wrap_w)
                .into_iter()
                .map(|l| Line::styled(l, label)),
        );
        if let Some(diff) = &detail.diff {
            for line in diff {
                let (mark, style) = match line.kind {
                    ApprovalDiffKind::Add => ("+ ", Style::default().fg(Color::Green)),
                    ApprovalDiffKind::Remove => ("- ", Style::default().fg(Color::Red)),
                    ApprovalDiffKind::Same => ("  ", Style::default()),
                };
                lines.push(Line::styled(format!("{INDENT}{mark}{}", line.text), style));
            }
        } else if detail.mono {
            lines.extend(
                detail
                    .value
                    .lines()
                    .map(|l| Line::styled(format!("{INDENT}{l}"), mono)),
            );
        } else {
            lines.extend(
                wrap(&detail.value, value_w)
                    .into_iter()
                    .map(|l| Line::raw(format!("{INDENT}{l}"))),
            );
        }
    }
    lines
}

/// Screen width of `text`.
fn text_width(text: &str) -> usize {
    Span::raw(text).width()
}

/// Break `text` into lines of at most `width` columns. Breaks at spaces
/// where possible, and in a word only if the word is too long for a line.
/// Keeps the line breaks of the text. A run of spaces becomes one space.
/// This is correct for plain text, because the host already makes each
/// run of white space one space. Do not use it for code or data.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for source in text.lines() {
        let mut line = String::new();
        let mut line_w = 0;
        for word in source.split(' ').filter(|w| !w.is_empty()) {
            let word_w = text_width(word);
            if line_w > 0 && line_w + 1 + word_w <= width {
                line.push(' ');
                line.push_str(word);
                line_w += 1 + word_w;
                continue;
            }
            if line_w > 0 {
                out.push(std::mem::take(&mut line));
                line_w = 0;
            }
            if word_w <= width {
                line.push_str(word);
                line_w = word_w;
                continue;
            }
            // The word is longer than a full line. Cut it at the width.
            for ch in word.chars() {
                let ch_w = text_width(ch.encode_utf8(&mut [0; 4]));
                if line_w + ch_w > width && line_w > 0 {
                    out.push(std::mem::take(&mut line));
                    line_w = 0;
                }
                line.push(ch);
                line_w += ch_w;
            }
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    //! Tests of the text wrap and the layout of the dialog.

    use super::*;
    use crate::approval::ApprovalDetail;

    /// Test that the wrap breaks at spaces, cuts words that are too long,
    /// and keeps the line breaks and empty lines of the text. Wide chars
    /// use two columns.
    ///   1. Wrap a text with short words, a long word and an empty line
    ///   2. Wrap a CJK word into a width that is not even
    ///   3. Check each line
    #[test]
    fn wrap_breaks_at_spaces_and_cuts_long_words() {
        assert_eq!(
            wrap("allow access to\n\nabcdefghij end", 8),
            ["allow", "access", "to", "", "abcdefgh", "ij end"]
        );
        assert_eq!(wrap("日本語テキスト", 5), ["日本", "語テ", "キス", "ト"]);
    }

    /// Test that the dialog draws on all small screen sizes with no panic,
    /// that the buttons stay in the screen, and that a dialog with no
    /// visible content shows only the deny button and a note.
    ///   1. Draw the short and the expanded form on each size up to 40x12
    ///   2. Check that each button rect is in the screen area
    ///   3. If no content row shows, check that only deny can be clicked
    ///   4. Check that a 3x1 screen shows the start of the deny button
    ///   5. Check that the note shows on a 40x3 screen
    #[test]
    fn dialog_on_tiny_screens_stays_in_area() {
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let mut approvals = Approvals::default();
        approvals.push(ApprovalRequest {
            title: "Allow network access to api.example.com?".into(),
            body: "The sandbox asks for access.".into(),
            details: vec![ApprovalDetail {
                label: "Command".into(),
                value: "curl https://api.example.com/".repeat(4),
                mono: true,
                diff: None,
            }],
            reply,
        });
        for expanded in [false, true] {
            approvals.expanded = expanded;
            for w in 0..=40 {
                for h in 0..=12 {
                    let area = Rect::new(0, 0, w, h);
                    let mut buf = Buffer::empty(area);
                    render_dialog(&approvals, area, &mut buf);
                    let l = approvals.layout.get();
                    for rect in [l.accept, l.deny, l.toggle] {
                        assert_eq!(rect.intersection(area), rect, "{w}x{h}");
                    }
                    if l.page == 0 {
                        assert!(l.accept.is_empty() && l.toggle.is_empty(), "{w}x{h}");
                    }
                }
            }
        }

        // A screen with no room for the frame still shows deny.
        let area = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);
        render_dialog(&approvals, area, &mut buf);
        assert!(!approvals.layout.get().deny.is_empty());
        assert_eq!(buf.content()[0].symbol(), "d");

        // A frame with room only for the buttons shows the note.
        let area = Rect::new(0, 0, 40, 3);
        let mut buf = Buffer::empty(area);
        render_dialog(&approvals, area, &mut buf);
        let text: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains(TOO_SMALL), "{text}");
        assert!(text.contains("deny") && !text.contains("accept"), "{text}");
    }
}
