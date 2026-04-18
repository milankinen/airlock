//! Network panel — rounded-border box with sub-tabs:
//! `Requests` (HTTP), `Connections` (raw TCP), and `Details` (shown on
//! demand when the user presses Enter on a selected row).
//!
//! Responsibilities of this module:
//! - `NetworkTab`: panel state (active sub-tab, counters, selection,
//!   open detail view).
//! - `NetworkWidget`: the panel's outer chrome (border, title, mode
//!   indicator, sub-tab bar, footer). Body rendering is delegated to
//!   the per-sub-tab widget modules.

mod chrome;
mod connections;
mod details;
mod footer;
mod requests;
mod row;

use std::cell::Cell;

pub use connections::ConnectionEntry;
pub use details::DetailView;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::Widget;
pub use requests::RequestEntry;

use crate::{NetworkEvent, Policy, TuiSettings};

/// Which network sub-tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkSubTab {
    #[default]
    Requests,
    Connections,
    /// Detail view for the currently-open entry. Only valid while
    /// `NetworkTab::details` is `Some`.
    Details,
}

/// Open-dropdown state for the policy selector. Closed when `None`.
pub struct PolicyDropdown {
    pub highlighted: Policy,
}

/// State for the network panel.
pub struct NetworkTab {
    pub sub_tab: NetworkSubTab,
    pub connections: Vec<ConnectionEntry>,
    pub requests: Vec<RequestEntry>,
    pub allowed_count: u32,
    pub denied_count: u32,
    /// Selection in the Requests sub-tab. Display index (0 = newest).
    selected_request: Option<usize>,
    /// Selection in the Connections sub-tab. Display index (0 = newest).
    selected_connection: Option<usize>,
    /// When `Some`, the Details sub-tab is open and shows this entry.
    details: Option<DetailView>,
    /// `Some` when the policy dropdown is open.
    dropdown: Option<PolicyDropdown>,
    /// Last rendered click rects for the sub-tab labels. Populated during
    /// render; consumed by mouse input.
    requests_rect: Cell<Option<Rect>>,
    connections_rect: Cell<Option<Rect>>,
    details_rect: Cell<Option<Rect>>,
    /// Click rect for the `×` close button on the Details sub-tab.
    details_close_rect: Cell<Option<Rect>>,
    /// Rect of the "policy: …" title anchor in the border line.
    policy_anchor: Cell<Option<Rect>>,
    /// Click rects for each dropdown row (in `Policy::ALL` order).
    dropdown_rects: Cell<Vec<(Policy, Rect)>>,
}

impl NetworkTab {
    pub fn new() -> Self {
        Self {
            sub_tab: NetworkSubTab::default(),
            connections: Vec::new(),
            requests: Vec::new(),
            allowed_count: 0,
            denied_count: 0,
            selected_request: None,
            selected_connection: None,
            details: None,
            dropdown: None,
            requests_rect: Cell::new(None),
            connections_rect: Cell::new(None),
            details_rect: Cell::new(None),
            details_close_rect: Cell::new(None),
            policy_anchor: Cell::new(None),
            dropdown_rects: Cell::new(Vec::new()),
        }
    }

    pub fn dropdown_open(&self) -> bool {
        self.dropdown.is_some()
    }

    /// Open the dropdown with `current` pre-highlighted.
    pub fn open_policy_dropdown(&mut self, current: Policy) {
        self.dropdown = Some(PolicyDropdown {
            highlighted: current,
        });
    }

    pub fn close_policy_dropdown(&mut self) {
        self.dropdown = None;
    }

    /// Move highlight up/down within `Policy::ALL`. `delta` is −1 / +1.
    pub fn nudge_policy_highlight(&mut self, delta: i32) {
        let Some(dd) = self.dropdown.as_mut() else {
            return;
        };
        let len = Policy::ALL.len() as i32;
        let idx = Policy::ALL
            .iter()
            .position(|p| *p == dd.highlighted)
            .unwrap_or(0) as i32;
        let next = ((idx + delta).rem_euclid(len)) as usize;
        dd.highlighted = Policy::ALL[next];
    }

    /// Highlighted entry, or `None` when closed.
    pub fn highlighted_policy(&self) -> Option<Policy> {
        self.dropdown.as_ref().map(|d| d.highlighted)
    }

    /// Hit-test a click against the policy title anchor.
    pub fn is_policy_anchor(&self, col: u16, row: u16) -> bool {
        self.policy_anchor.get().is_some_and(|r| {
            col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
        })
    }

    /// Hit-test a click against dropdown rows; returns the clicked policy.
    pub fn dropdown_row_at(&self, col: u16, row: u16) -> Option<Policy> {
        let rects = self.dropdown_rects.take();
        let hit = rects.iter().find_map(|(p, r)| {
            if col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height {
                Some(*p)
            } else {
                None
            }
        });
        self.dropdown_rects.set(rects);
        hit
    }

    pub fn details_open(&self) -> bool {
        self.details.is_some()
    }

    /// Append an event to the matching sub-tab, capped by `settings`.
    pub fn push_event(&mut self, ev: NetworkEvent, settings: &TuiSettings) {
        match ev {
            NetworkEvent::Connect(info) => {
                self.bump_count(info.allowed);
                self.connections.push(ConnectionEntry::from_info(&info));
                on_push_selection(&mut self.selected_connection, self.connections.len());
                cap_entries(
                    &mut self.connections,
                    settings.max_tcp_connections,
                    &mut self.selected_connection,
                );
            }
            NetworkEvent::Request(info) => {
                self.bump_count(info.allowed);
                self.requests.push(RequestEntry::from_info(&info));
                on_push_selection(&mut self.selected_request, self.requests.len());
                cap_entries(
                    &mut self.requests,
                    settings.max_http_requests,
                    &mut self.selected_request,
                );
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

    /// Jump directly to the given sub-tab and close any open details view.
    /// The caller is responsible for passing `Requests` or `Connections` —
    /// `Details` is opened via `open_details`.
    pub fn select_sub_tab(&mut self, tab: NetworkSubTab) {
        if tab == NetworkSubTab::Details {
            return;
        }
        self.details = None;
        self.sub_tab = tab;
    }

    /// Cycle between the Requests and Connections sub-tabs. If the Details
    /// sub-tab is active, return to the owning parent.
    pub fn toggle_sub_tab(&mut self) {
        let target = match self.sub_tab {
            NetworkSubTab::Requests => NetworkSubTab::Connections,
            NetworkSubTab::Connections => NetworkSubTab::Requests,
            NetworkSubTab::Details => {
                self.details
                    .as_ref()
                    .map_or(NetworkSubTab::Requests, |d| match d {
                        DetailView::Request(_) => NetworkSubTab::Requests,
                        DetailView::Connection(_) => NetworkSubTab::Connections,
                    })
            }
        };
        self.select_sub_tab(target);
    }

    /// Return the sub-tab whose rendered label rect contains `(col, row)`,
    /// or `None` if the click was outside any label.
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
        } else if hit(self.details_rect.get()) {
            Some(NetworkSubTab::Details)
        } else {
            None
        }
    }

    /// Hit-test a click against the `×` close button on the Details sub-tab.
    pub fn is_details_close(&self, col: u16, row: u16) -> bool {
        self.details_close_rect.get().is_some_and(|r| {
            col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
        })
    }

    // ── Selection helpers ───────────────────────────────────

    /// Move the selection up by one row (toward the newest entry).
    pub fn select_up(&mut self) {
        self.move_selection(-1);
    }

    /// Move the selection down by one row (toward the oldest entry).
    pub fn select_down(&mut self) {
        self.move_selection(1);
    }

    pub fn select_page_up(&mut self) {
        self.move_selection(-20);
    }

    pub fn select_page_down(&mut self) {
        self.move_selection(20);
    }

    /// Jump to the newest entry.
    pub fn select_newest(&mut self) {
        if self.list_len() > 0 {
            self.set_selection(0);
        }
    }

    /// Jump to the oldest entry.
    pub fn select_oldest(&mut self) {
        let len = self.list_len();
        if len > 0 {
            self.set_selection(len - 1);
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let cur = self.current_selection().unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, (len as i32) - 1) as usize;
        self.set_selection(next);
    }

    fn list_len(&self) -> usize {
        match self.sub_tab {
            NetworkSubTab::Requests => self.requests.len(),
            NetworkSubTab::Connections => self.connections.len(),
            NetworkSubTab::Details => 0,
        }
    }

    fn current_selection(&self) -> Option<usize> {
        match self.sub_tab {
            NetworkSubTab::Requests => self.selected_request,
            NetworkSubTab::Connections => self.selected_connection,
            NetworkSubTab::Details => None,
        }
    }

    fn set_selection(&mut self, idx: usize) {
        match self.sub_tab {
            NetworkSubTab::Requests => self.selected_request = Some(idx),
            NetworkSubTab::Connections => self.selected_connection = Some(idx),
            NetworkSubTab::Details => {}
        }
    }

    /// Open the Details sub-tab with a snapshot of the currently selected
    /// entry. No-op when nothing is selected.
    pub fn open_details(&mut self) {
        match self.sub_tab {
            NetworkSubTab::Requests => {
                if let Some(sel) = self.selected_request
                    && let Some(entry) = display_nth(&self.requests, sel)
                {
                    self.details = Some(DetailView::Request(entry.clone()));
                    self.sub_tab = NetworkSubTab::Details;
                }
            }
            NetworkSubTab::Connections => {
                if let Some(sel) = self.selected_connection
                    && let Some(entry) = display_nth(&self.connections, sel)
                {
                    self.details = Some(DetailView::Connection(entry.clone()));
                    self.sub_tab = NetworkSubTab::Details;
                }
            }
            NetworkSubTab::Details => {}
        }
    }

    /// Close the Details sub-tab and return to its parent sub-tab.
    pub fn close_details(&mut self) {
        let parent = self
            .details
            .as_ref()
            .map_or(NetworkSubTab::Requests, |d| match d {
                DetailView::Request(_) => NetworkSubTab::Requests,
                DetailView::Connection(_) => NetworkSubTab::Connections,
            });
        self.details = None;
        self.sub_tab = parent;
    }
}

/// Return the nth entry in display order (0 = newest = last vec entry).
fn display_nth<T>(vec: &[T], display_idx: usize) -> Option<&T> {
    vec.len()
        .checked_sub(1)
        .and_then(|last| last.checked_sub(display_idx))
        .and_then(|vec_idx| vec.get(vec_idx))
}

/// Update a display-index selection after appending a new entry. Selection
/// at 0 (newest) stays at 0 — "follow newest" semantics. Selection at `n>0`
/// shifts to `n+1` so it keeps pointing to the same underlying entry.
fn on_push_selection(selected: &mut Option<usize>, new_len: usize) {
    match *selected {
        None => {
            if new_len > 0 {
                *selected = Some(0);
            }
        }
        Some(0) => {} // track newest
        Some(n) => *selected = Some((n + 1).min(new_len.saturating_sub(1))),
    }
}

/// Evict oldest entries (front of vec) until `vec.len() <= max`. Keeps
/// display-index selection valid by clamping to the new last display index.
fn cap_entries<T>(vec: &mut Vec<T>, max: usize, selected: &mut Option<usize>) {
    while vec.len() > max {
        vec.remove(0);
    }
    let len = vec.len();
    if len == 0 {
        *selected = None;
    } else if let Some(n) = *selected
        && n >= len
    {
        *selected = Some(len - 1);
    }
}

/// Renders the network panel (border + title + sub-tabs + body + footer).
pub struct NetworkWidget<'a> {
    tab: &'a NetworkTab,
    policy: crate::Policy,
}

impl<'a> NetworkWidget<'a> {
    pub fn new(tab: &'a NetworkTab, policy: crate::Policy) -> Self {
        Self { tab, policy }
    }
}

impl Widget for NetworkWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height < 5 || area.width < 20 {
            return;
        }

        let (inner, anchor) = chrome::render_frame(area, self.policy, buf);
        self.tab.policy_anchor.set(Some(anchor));
        if inner.height < 3 {
            return;
        }

        let [tabs_area, body_area, footer_area] = Layout::vertical([
            Constraint::Length(3), // blank top margin + sub-tab row + separator
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        let details_label = self.tab.details.as_ref().map(|d| match d {
            DetailView::Request(_) => "Request details",
            DetailView::Connection(_) => "Connection details",
        });
        let rects = chrome::render_sub_tabs(tabs_area, self.tab.sub_tab, details_label, buf);
        self.tab.requests_rect.set(Some(rects.requests));
        self.tab.connections_rect.set(Some(rects.connections));
        self.tab.details_rect.set(rects.details);
        self.tab.details_close_rect.set(rects.details_close);

        match self.tab.sub_tab {
            NetworkSubTab::Requests => {
                requests::RequestsWidget::new(&self.tab.requests, self.tab.selected_request)
                    .render(body_area, buf);
            }
            NetworkSubTab::Connections => {
                connections::ConnectionsWidget::new(
                    &self.tab.connections,
                    self.tab.selected_connection,
                )
                .render(body_area, buf);
            }
            NetworkSubTab::Details => {
                if let Some(d) = self.tab.details.as_ref() {
                    details::DetailsWidget::new(d).render(body_area, buf);
                }
            }
        }

        footer::render_footer(
            footer_area,
            self.tab.allowed_count,
            self.tab.denied_count,
            buf,
        );

        // Dropdown overlay renders last so it paints on top of body content.
        if let Some(dropdown) = self.tab.dropdown.as_ref() {
            let rows = chrome::render_policy_dropdown(area, anchor, dropdown.highlighted, buf);
            self.tab.dropdown_rects.set(rows);
        } else {
            self.tab.dropdown_rects.set(Vec::new());
        }
    }
}
