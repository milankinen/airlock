//! Details sub-tab body — shows a snapshot of one request or connection.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use super::connections::ConnectionEntry;
use super::requests::RequestEntry;
use super::row::{format_timestamp, format_transfer};

/// Which entry the details view is showing.
#[derive(Clone)]
pub enum DetailView {
    Request(RequestEntry),
    Connection(ConnectionEntry),
}

pub struct DetailsWidget<'a> {
    view: &'a DetailView,
    scroll: u16,
    /// Called with the largest useful scroll offset for this render, so
    /// the owning tab can clamp its key handling to the real content.
    report_max_scroll: &'a dyn Fn(u16),
}

impl<'a> DetailsWidget<'a> {
    pub fn new(view: &'a DetailView, scroll: u16, report_max_scroll: &'a dyn Fn(u16)) -> Self {
        Self {
            view,
            scroll,
            report_max_scroll,
        }
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
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });

        // `line_count` measures *after* wrapping, so a single long header
        // that folds into four rows costs four lines of scroll — which is
        // what the user actually sees.
        let content = u16::try_from(paragraph.line_count(area.width)).unwrap_or(u16::MAX);
        let max_scroll = content.saturating_sub(area.height);
        (self.report_max_scroll)(max_scroll);

        // Clamp here too: the offset was chosen against the *previous*
        // render's width, and a resize can shrink the content under it.
        let scroll = self.scroll.min(max_scroll);
        paragraph.scroll((scroll, 0)).render(area, buf);
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
    out.push(field("Received", Span::raw(format_timestamp(r.timestamp))));
    out.push(field(
        "Method",
        Span::styled(
            r.method.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ));
    out.push(field("Target", Span::raw(format!("{}:{}", r.host, r.port))));
    out.push(field("Path", Span::raw(r.path.clone())));
    out.push(Line::from(""));
    out.push(section("Request headers"));
    push_headers(&mut out, &r.headers);

    out.push(Line::from(""));
    out.push(section("Response"));
    if let Some(status) = r.status {
        out.push(field(
            "Status",
            Span::styled(
                status_code_label(status),
                Style::default()
                    .fg(status_code_color(status))
                    .add_modifier(Modifier::BOLD),
            ),
        ));
        out.push(Line::from(""));
        out.push(section("Response headers"));
        push_headers(&mut out, &r.response_headers);
    } else {
        // Either still in flight, or the connection died before a reply
        // came back — the monitor can't tell those apart after the fact.
        out.push(Line::from(Span::styled(
            "    (no response yet)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    out
}

/// Status line as `404 Not Found`, falling back to the bare number for
/// codes with no canonical reason phrase.
fn status_code_label(status: u16) -> String {
    match http_reason(status) {
        Some(reason) => format!("{status} {reason}"),
        None => status.to_string(),
    }
}

/// Green for 2xx, cyan for 3xx, yellow for 4xx, red for 5xx.
fn status_code_color(status: u16) -> Color {
    match status {
        200..=299 => Color::Green,
        300..=399 => Color::Cyan,
        400..=499 => Color::Yellow,
        500..=599 => Color::Red,
        _ => Color::Gray,
    }
}

/// Reason phrases for the codes a sandboxed workload realistically sees.
/// Deliberately not exhaustive — unknown codes render bare.
fn http_reason(status: u16) -> Option<&'static str> {
    Some(match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        418 => "I'm a teapot",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => return None,
    })
}

fn section(title: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            title.to_string(),
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ),
    ])
}

fn push_headers(out: &mut Vec<Line<'static>>, headers: &[(String, String)]) {
    if headers.is_empty() {
        out.push(Line::from(Span::styled(
            "    (none)",
            Style::default().fg(Color::DarkGray),
        )));
        return;
    }
    for (name, value) in headers {
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("{name}: "), Style::default().fg(Color::DarkGray)),
            Span::raw(value.clone()),
        ]));
    }
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
        field("Connected", Span::raw(format_timestamp(c.timestamp))),
    ];
    if let Some(ts) = c.disconnected_at {
        out.push(field("Disconnected", Span::raw(format_timestamp(ts))));
    }
    out.push(field("Target", Span::raw(format!("{}:{}", c.host, c.port))));
    out.push(field(
        "Sent",
        Span::raw(format!("{} ({} bytes)", format_transfer(c.up), c.up)),
    ));
    out.push(field(
        "Received",
        Span::raw(format!("{} ({} bytes)", format_transfer(c.down), c.down)),
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
