//! Footer row for the network panel — allowed/denied counts, plus a
//! right-aligned hint while the details view is open.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

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

    // Click-to-select has no visual affordance of its own, so the details
    // view spells it out. Rendered second, right-aligned over the same
    // row — the counts sit far enough left that they don't collide.
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
