//! Footer row of the network panel.
//!
//! Shows the number of allowed and denied entries in the visible list.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

/// Draw the footer row.
/// Args:
///  - `area`: Area of the footer row
///  - `allowed`: Number of allowed entries
///  - `denied`: Number of denied entries
///  - `details_open`: True if the details view is open. Then the footer
///    also shows a text selection hint on the right.
///  - `buf`: Buffer to draw into.
pub fn render_footer(area: Rect, allowed: u32, denied: u32, details_open: bool, buf: &mut Buffer) {
    let line = Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{allowed}"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" allowed  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{denied}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" denied", Style::default().fg(Color::DarkGray)),
    ]);
    Paragraph::new(line).render(area, buf);

    // Nothing on the screen shows that a click selects text. Thus the details
    // view tells it with this hint. Draw it second, aligned to the right on
    // the same row. The counts are far enough to the left, so the two texts
    // do not overlap.
    if details_open {
        let hint = Line::from(Span::styled(
            "click to select text  ",
            Style::default().fg(Color::DarkGray),
        ));
        Paragraph::new(hint)
            .alignment(Alignment::Right)
            .render(area, buf);
    }
}
