//! Details sub-tab body — shows a snapshot of one request or connection.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use super::connections::ConnectionEntry;
use super::requests::RequestEntry;
use super::row::format_timestamp;

/// Which entry the details view is showing.
#[derive(Clone)]
pub enum DetailView {
    Request(RequestEntry),
    Connection(ConnectionEntry),
}

pub struct DetailsWidget<'a> {
    view: &'a DetailView,
}

impl<'a> DetailsWidget<'a> {
    pub fn new(view: &'a DetailView) -> Self {
        Self { view }
    }
}

impl Widget for DetailsWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let lines = match self.view {
            DetailView::Request(r) => request_lines(r),
            DetailView::Connection(c) => connection_lines(c),
        };
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }
}

fn request_lines(r: &RequestEntry) -> Vec<Line<'static>> {
    let status_color = if r.allowed { Color::Green } else { Color::Red };
    let status_text = if r.allowed { "Allowed" } else { "Denied" };
    let mut out = Vec::new();
    out.push(Line::from(""));
    out.push(field(
        "Status",
        Span::styled(
            status_text,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
    ));
    out.push(field(
        "Received",
        Span::styled(
            format_timestamp(r.timestamp),
            Style::default().fg(Color::Gray),
        ),
    ));
    out.push(field(
        "Method",
        Span::styled(
            r.method.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ));
    out.push(field(
        "Target",
        Span::styled(
            format!("{}:{}", r.host, r.port),
            Style::default().fg(Color::Gray),
        ),
    ));
    out.push(field(
        "Path",
        Span::styled(r.path.clone(), Style::default().fg(Color::Gray)),
    ));
    out.push(Line::from(""));
    out.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Headers",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ),
    ]));
    if r.headers.is_empty() {
        out.push(Line::from(Span::styled(
            "    (none)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (name, value) in &r.headers {
            out.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("{name}: "), Style::default().fg(Color::DarkGray)),
                Span::styled(value.clone(), Style::default().fg(Color::Gray)),
            ]));
        }
    }
    out
}

fn connection_lines(c: &ConnectionEntry) -> Vec<Line<'static>> {
    let status_color = if c.allowed { Color::Green } else { Color::Red };
    let status_text = if c.allowed { "Allowed" } else { "Denied" };
    let (state_text, state_color) = if c.allowed && c.disconnected_at.is_none() {
        ("Open", Color::Green)
    } else {
        ("Closed", Color::DarkGray)
    };
    let mut out = vec![
        Line::from(""),
        field(
            "Status",
            Span::styled(
                status_text,
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        field(
            "State",
            Span::styled(
                state_text,
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        field(
            "Connected",
            Span::styled(
                format_timestamp(c.timestamp),
                Style::default().fg(Color::Gray),
            ),
        ),
    ];
    if let Some(ts) = c.disconnected_at {
        out.push(field(
            "Disconnected",
            Span::styled(format_timestamp(ts), Style::default().fg(Color::Gray)),
        ));
    }
    out.push(field(
        "Target",
        Span::styled(
            format!("{}:{}", c.host, c.port),
            Style::default().fg(Color::Gray),
        ),
    ));
    out
}

fn field(label: &str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{label:<12}"), Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        value,
    ])
}
