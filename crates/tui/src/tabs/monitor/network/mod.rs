//! Network panel — rounded-border box with two sub-tabs:
//! `Requests` (HTTP) and `Connections` (raw TCP).
//!
//! Responsibilities of this module:
//! - `NetworkTab`: panel state (active sub-tab, counters, scroll).
//! - `NetworkWidget`: the panel's outer chrome (border, title, mode
//!   indicator, sub-tab bar, footer). Body rendering is delegated to
//!   the per-sub-tab widget modules.

mod chrome;
mod connections;
mod footer;
mod requests;
mod row;

use std::cell::Cell;

pub use connections::ConnectionEntry;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::Widget;
pub use requests::RequestEntry;

use crate::NetworkEvent;

/// Which network sub-tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkSubTab {
    Requests,
    #[default]
    Connections,
}

/// State for the network panel.
pub struct NetworkTab {
    pub sub_tab: NetworkSubTab,
    pub connections: Vec<ConnectionEntry>,
    pub requests: Vec<RequestEntry>,
    pub allowed_count: u32,
    pub denied_count: u32,
    scroll_offset: usize,
    /// Last rendered click rects for the Requests / Connections sub-tab
    /// labels. Populated during render; consumed by mouse input.
    requests_rect: Cell<Option<Rect>>,
    connections_rect: Cell<Option<Rect>>,
}

impl NetworkTab {
    pub fn new() -> Self {
        Self {
            sub_tab: NetworkSubTab::default(),
            connections: Vec::new(),
            requests: Vec::new(),
            allowed_count: 0,
            denied_count: 0,
            scroll_offset: 0,
            requests_rect: Cell::new(None),
            connections_rect: Cell::new(None),
        }
    }

    pub fn push_event(&mut self, ev: NetworkEvent) {
        match ev {
            NetworkEvent::Connect {
                host,
                port,
                allowed,
            } => {
                self.bump_count(allowed);
                self.connections
                    .push(ConnectionEntry::new(host, port, allowed));
            }
            NetworkEvent::Request {
                method,
                path,
                host,
                port,
                allowed,
            } => {
                self.bump_count(allowed);
                self.requests
                    .push(RequestEntry::new(method, path, host, port, allowed));
            }
        }
    }

    fn bump_count(&mut self, allowed: bool) {
        if allowed {
            self.allowed_count += 1;
        } else {
            self.denied_count += 1;
        }
    }

    pub fn total_count(&self) -> u32 {
        self.allowed_count + self.denied_count
    }

    /// Cycle to the next sub-tab. With two sub-tabs this is also "previous".
    pub fn toggle_sub_tab(&mut self) {
        self.sub_tab = match self.sub_tab {
            NetworkSubTab::Requests => NetworkSubTab::Connections,
            NetworkSubTab::Connections => NetworkSubTab::Requests,
        };
        self.scroll_offset = 0;
    }

    /// Jump directly to the given sub-tab and reset scroll.
    pub fn select_sub_tab(&mut self, tab: NetworkSubTab) {
        if self.sub_tab != tab {
            self.sub_tab = tab;
            self.scroll_offset = 0;
        }
    }

    /// Return the sub-tab whose rendered label rect contains `(col, row)`,
    /// or `None` if the click was outside either label.
    pub fn sub_tab_at(&self, col: u16, row: u16) -> Option<NetworkSubTab> {
        let hit = |r: Option<Rect>| {
            r.is_some_and(|r| {
                col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
            })
        };
        if hit(self.requests_rect.get()) {
            Some(NetworkSubTab::Requests)
        } else if hit(self.connections_rect.get()) {
            Some(NetworkSubTab::Connections)
        } else {
            None
        }
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: usize) {
        let max = self.entry_count().saturating_sub(1);
        self.scroll_offset = (self.scroll_offset + n).min(max);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = self.entry_count().saturating_sub(1);
    }

    fn entry_count(&self) -> usize {
        match self.sub_tab {
            NetworkSubTab::Requests => self.requests.len(),
            NetworkSubTab::Connections => self.connections.len(),
        }
    }
}

/// Renders the network panel (border + title + sub-tabs + body + footer).
pub struct NetworkWidget<'a> {
    tab: &'a NetworkTab,
    /// Label shown in the rounded-border title bar (display-only in current design).
    mode_label: &'a str,
}

impl<'a> NetworkWidget<'a> {
    pub fn new(tab: &'a NetworkTab, mode_label: &'a str) -> Self {
        Self { tab, mode_label }
    }
}

impl Widget for NetworkWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height < 5 || area.width < 20 {
            return;
        }

        let inner = chrome::render_frame(area, self.mode_label, buf);
        if inner.height < 3 {
            return;
        }

        let [tabs_area, body_area, footer_area] = Layout::vertical([
            Constraint::Length(3), // blank top margin + sub-tab row + separator
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        let (req_rect, conn_rect) = chrome::render_sub_tabs(tabs_area, self.tab.sub_tab, buf);
        self.tab.requests_rect.set(Some(req_rect));
        self.tab.connections_rect.set(Some(conn_rect));

        match self.tab.sub_tab {
            NetworkSubTab::Requests => {
                requests::RequestsWidget::new(&self.tab.requests, self.tab.scroll_offset)
                    .render(body_area, buf);
            }
            NetworkSubTab::Connections => {
                connections::ConnectionsWidget::new(&self.tab.connections, self.tab.scroll_offset)
                    .render(body_area, buf);
            }
        }

        footer::render_footer(
            footer_area,
            self.tab.allowed_count,
            self.tab.denied_count,
            buf,
        );
    }
}
