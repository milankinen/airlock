//! Network tab — live connection log with counters and scroll.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table, Widget};

use crate::NetworkEvent;

/// A single log entry displayed in the network tab.
struct LogEntry {
    host: String,
    port: u16,
    allowed: bool,
}

/// State for the network monitoring tab.
pub struct NetworkTab {
    entries: Vec<LogEntry>,
    pub allowed_count: u32,
    pub denied_count: u32,
    scroll_offset: usize,
}

impl NetworkTab {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            allowed_count: 0,
            denied_count: 0,
            scroll_offset: 0,
        }
    }

    pub fn push_event(&mut self, ev: NetworkEvent) {
        match ev {
            NetworkEvent::Connect {
                host,
                port,
                allowed,
            } => {
                if allowed {
                    self.allowed_count += 1;
                } else {
                    self.denied_count += 1;
                }
                self.entries.push(LogEntry {
                    host,
                    port,
                    allowed,
                });
            }
        }
    }

    pub fn total_count(&self) -> u32 {
        self.allowed_count + self.denied_count
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: usize) {
        let max = self.entries.len().saturating_sub(1);
        self.scroll_offset = (self.scroll_offset + n).min(max);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = self.entries.len().saturating_sub(1);
    }
}

/// Widget that renders the network tab content.
pub struct NetworkWidget<'a> {
    tab: &'a NetworkTab,
    policy: &'a str,
}

impl<'a> NetworkWidget<'a> {
    pub fn new(tab: &'a NetworkTab, policy: &'a str) -> Self {
        Self { tab, policy }
    }
}

impl Widget for NetworkWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height < 4 {
            return;
        }

        let [header_area, table_area, summary_area] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);

        // Policy header
        let policy_line = Line::from(vec![
            Span::styled(" Policy: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(self.policy, Style::default().fg(Color::Cyan)),
        ]);
        Paragraph::new(policy_line).render(header_area, buf);

        // Connection log table
        let visible_rows = table_area.height.saturating_sub(2) as usize; // account for header + borders
        let start = self.tab.scroll_offset;
        let end = (start + visible_rows).min(self.tab.entries.len());

        let rows: Vec<Row> = self.tab.entries[start..end]
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let idx = start + i + 1;
                let status = if entry.allowed { "ALLOW" } else { "DENY" };
                let style = if entry.allowed {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Red)
                };
                Row::new(vec![
                    format!("{idx:>4}"),
                    format!("{}:{}", entry.host, entry.port),
                    status.to_string(),
                ])
                .style(style)
            })
            .collect();

        let header = Row::new(vec!["#", "Target", "Status"])
            .style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .bottom_margin(0);

        let widths = [
            Constraint::Length(5),
            Constraint::Min(20),
            Constraint::Length(6),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().borders(Borders::TOP));
        Widget::render(table, table_area, buf);

        // Summary bar
        let summary = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                format!("{} ", self.tab.allowed_count),
                Style::default().fg(Color::Green),
            ),
            Span::raw("allowed  "),
            Span::styled(
                format!("{} ", self.tab.denied_count),
                Style::default().fg(Color::Red),
            ),
            Span::raw("denied  "),
            Span::styled(
                format!("{} ", self.tab.total_count()),
                Style::default().fg(Color::White),
            ),
            Span::raw("total"),
        ]);
        Paragraph::new(summary).render(summary_area, buf);
    }
}
