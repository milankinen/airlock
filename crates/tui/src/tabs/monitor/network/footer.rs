//! Footer row for the network panel — allowed/denied counts.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

pub fn render_footer(area: Rect, allowed: u32, denied: u32, buf: &mut Buffer) {
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
}
