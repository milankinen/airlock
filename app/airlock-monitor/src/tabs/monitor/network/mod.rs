//! Network panel of the Monitor tab.
//!
//! Shows the network activity of the sandbox in sub-tabs:
//!  * `Requests`: HTTP requests
//!  * `Connections`: raw TCP connections
//!  * `Details`: details of one entry. Shows when the user opens a selected
//!    row.
//!
//! The user can also change the network policy from this panel.

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

/// Sub-tab of the network panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkSubTab {
    /// List of HTTP requests.
    #[default]
    Requests,
    /// List of TCP connections.
    Connections,
    /// Details of the open entry. Valid only when `NetworkTab::details` is
    /// `Some`.
    Details,
}

/// State of the open policy dropdown. `None` means that it is closed.
pub struct PolicyDropdown {
    /// Policy that the dropdown highlights.
    pub highlighted: Policy,
}

/// State of the network panel.
pub struct NetworkTab {
    /// Sub-tab that is visible now.
    pub sub_tab: NetworkSubTab,
    /// Recent TCP connections, oldest first.
    pub connections: Vec<ConnectionEntry>,
    /// Recent HTTP requests, oldest first.
    pub requests: Vec<RequestEntry>,
    /// Number of allowed connections in the session.
    ///
    /// The four counters increment on each `Connect` or `Request` event. A
    /// `Response` that a middleware denied moves one request from allowed
    /// to denied. The counters are separate from the lists, so they keep
    /// their values when the lists are full.
    pub connection_allowed: u32,
    /// Number of denied connections in the session.
    pub connection_denied: u32,
    /// Number of allowed requests in the session.
    pub request_allowed: u32,
    /// Number of denied requests in the session.
    pub request_denied: u32,
    /// Selection in the Requests sub-tab, as a display index (0 = newest).
    selected_request: Option<usize>,
    /// Selection in the Connections sub-tab, as a display index
    /// (0 = newest).
    selected_connection: Option<usize>,
    /// When `Some`, the Details sub-tab is open and shows this entry.
    details: Option<DetailView>,
    /// First visible wrapped line of the details body. Set to 0 each time a
    /// details view opens.
    details_scroll: u16,
    /// Maximum scroll offset of the details body: the offset where the last
    /// line is at the bottom of the viewport. It is
    /// `content_lines - viewport_height`, or 0 when all content fits. The
    /// render writes it, because only the render knows the wrap width. The
    /// scroll actions read it.
    details_max_scroll: Cell<u16>,
    /// `Some` when the policy dropdown is open.
    dropdown: Option<PolicyDropdown>,
    /// Click rects of the sub-tab labels from the last render. The render
    /// writes them and the mouse input reads them.
    requests_rect: Cell<Option<Rect>>,
    connections_rect: Cell<Option<Rect>>,
    details_rect: Cell<Option<Rect>>,
    /// Click rect of the `×` close button on the Details sub-tab.
    details_close_rect: Cell<Option<Rect>>,
    /// Rect of the "policy: …" title in the border line.
    policy_anchor: Cell<Option<Rect>>,
    /// Click rects of the dropdown rows (in `Policy::ALL` order).
    dropdown_rects: Cell<Vec<(Policy, Rect)>>,
}

impl NetworkTab {
    /// Create an empty network panel state.
    pub fn new() -> Self {
        Self {
            sub_tab: NetworkSubTab::default(),
            connections: Vec::new(),
            requests: Vec::new(),
            connection_allowed: 0,
            connection_denied: 0,
            request_allowed: 0,
            request_denied: 0,
            selected_request: None,
            selected_connection: None,
            details: None,
            details_scroll: 0,
            details_max_scroll: Cell::new(0),
            dropdown: None,
            requests_rect: Cell::new(None),
            connections_rect: Cell::new(None),
            details_rect: Cell::new(None),
            details_close_rect: Cell::new(None),
            policy_anchor: Cell::new(None),
            dropdown_rects: Cell::new(Vec::new()),
        }
    }

    /// True if the policy dropdown is open.
    pub fn dropdown_open(&self) -> bool {
        self.dropdown.is_some()
    }

    /// Open the policy dropdown, with `current` highlighted.
    pub fn open_policy_dropdown(&mut self, current: Policy) {
        self.dropdown = Some(PolicyDropdown {
            highlighted: current,
        });
    }

    /// Close the policy dropdown.
    pub fn close_policy_dropdown(&mut self) {
        self.dropdown = None;
    }

    /// Move the dropdown highlight in [`Policy::ALL`] by `delta` rows (-1 for
    /// up, +1 for down). The highlight wraps at the ends. No effect when the
    /// dropdown is closed.
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

    /// Highlighted policy, or `None` when the dropdown is closed.
    pub fn highlighted_policy(&self) -> Option<Policy> {
        self.dropdown.as_ref().map(|d| d.highlighted)
    }

    /// True if the click position (`col`, `row`) is on the policy title.
    pub fn is_policy_anchor(&self, col: u16, row: u16) -> bool {
        self.policy_anchor.get().is_some_and(|r| {
            col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
        })
    }

    /// Policy of the dropdown row at the click position (`col`, `row`), or
    /// `None` if the click is not on a row.
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

    /// True if the Details sub-tab is open.
    pub fn details_open(&self) -> bool {
        self.details.is_some()
    }

    /// Apply a network event to the panel state.
    /// Args:
    ///  - `ev`: Network event from the host
    ///  - `settings`: Settings with the maximum list sizes.
    ///
    /// Connect and request events add entries to their lists. Other events
    /// update the existing entries and the open details view.
    pub fn push_event(&mut self, ev: NetworkEvent, settings: &TuiSettings) {
        match ev {
            NetworkEvent::Connect(info) => {
                bump(
                    info.allowed,
                    &mut self.connection_allowed,
                    &mut self.connection_denied,
                );
                self.connections.push(ConnectionEntry::from_info(&info));
                on_push_selection(&mut self.selected_connection, self.connections.len());
                cap_entries(
                    &mut self.connections,
                    settings.max_tcp_connections,
                    &mut self.selected_connection,
                );
            }
            NetworkEvent::Disconnect(info) => {
                // The list limit may have already removed the entry. That is
                // correct: then there is no row to update.
                if let Some(entry) = self.connections.iter_mut().find(|c| c.id == info.id) {
                    entry.disconnected_at = Some(info.timestamp);
                }
                // Update the open details view separately from the list. The
                // view keeps a copy by ID, and the copy stays after the list
                // removes the row. Thus it must update also when the row is
                // gone. If not, the pane always shows an old "Open" state.
                if let Some(DetailView::Connection(open)) = self.details.as_mut()
                    && open.id == info.id
                {
                    open.disconnected_at = Some(info.timestamp);
                }
            }
            NetworkEvent::Traffic(info) => {
                // As for `Disconnect`, the list limit may have already
                // removed the row. Then there is nothing to update.
                if let Some(entry) = self.connections.iter_mut().find(|c| c.id == info.id) {
                    entry.up = info.up;
                    entry.down = info.down;
                }
                // The open details copy follows the connection by ID and can
                // stay after its row is removed. Thus update it separately
                // from the list. If not, the byte counts stop after removal.
                if let Some(DetailView::Connection(open)) = self.details.as_mut()
                    && open.id == info.id
                {
                    open.up = info.up;
                    open.down = info.down;
                }
            }
            NetworkEvent::Request(info) => {
                bump(
                    info.allowed,
                    &mut self.request_allowed,
                    &mut self.request_denied,
                );
                self.requests.push(RequestEntry::from_info(&info));
                on_push_selection(&mut self.selected_request, self.requests.len());
                cap_entries(
                    &mut self.requests,
                    settings.max_http_requests,
                    &mut self.selected_request,
                );
            }
            NetworkEvent::Response(info) => {
                // A middleware denial changes a request that was already
                // counted as allowed. Move the count here, not in the row
                // update, because the list limit may have removed the row.
                if info.denied {
                    self.request_allowed = self.request_allowed.saturating_sub(1);
                    self.request_denied += 1;
                }
                if let Some(entry) = self.requests.iter_mut().find(|r| r.id == info.id) {
                    entry.apply_response(&info);
                }
                // Update the open details view separately from the list. A
                // removed request still shows its copy here. Thus the user
                // sees the response without a new open of the row.
                if let Some(DetailView::Request(open)) = self.details.as_mut()
                    && open.id == info.id
                {
                    open.apply_response(&info);
                }
            }
        }
    }

    /// Counters (allowed, denied) for the visible sub-tab. The Details
    /// sub-tab uses the counters of its parent sub-tab.
    pub fn visible_counts(&self) -> (u32, u32) {
        let tab = match self.sub_tab {
            NetworkSubTab::Details => {
                self.details
                    .as_ref()
                    .map_or(NetworkSubTab::Requests, |d| match d {
                        DetailView::Request(_) => NetworkSubTab::Requests,
                        DetailView::Connection(_) => NetworkSubTab::Connections,
                    })
            }
            other => other,
        };
        match tab {
            NetworkSubTab::Requests => (self.request_allowed, self.request_denied),
            NetworkSubTab::Connections => (self.connection_allowed, self.connection_denied),
            NetworkSubTab::Details => (0, 0),
        }
    }

    /// Go to the sub-tab `tab` and close the open details view.
    ///
    /// Use only `Requests` or `Connections`. `Details` has no effect. To
    /// open it, use [`NetworkTab::open_details`].
    pub fn select_sub_tab(&mut self, tab: NetworkSubTab) {
        if tab == NetworkSubTab::Details {
            return;
        }
        self.details = None;
        self.sub_tab = tab;
    }

    /// Change between the Requests and Connections sub-tabs. If the Details
    /// sub-tab is active, go back to its parent sub-tab.
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

    /// Sub-tab whose label contains the click position (`col`, `row`), or
    /// `None` if the click is not on a label.
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

    /// True if the click position (`col`, `row`) is on the `×` close button
    /// of the Details sub-tab.
    pub fn is_details_close(&self, col: u16, row: u16) -> bool {
        self.details_close_rect.get().is_some_and(|r| {
            col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
        })
    }

    // ── Selection helpers ───────────────────────────────────

    /// Move the selection up one row (toward the newest entry).
    pub fn select_up(&mut self) {
        self.move_selection(-1);
    }

    /// Move the selection down one row (toward the oldest entry).
    pub fn select_down(&mut self) {
        self.move_selection(1);
    }

    /// Move the selection up one page (toward the newest entry).
    pub fn select_page_up(&mut self) {
        self.move_selection(-20);
    }

    /// Move the selection down one page (toward the oldest entry).
    pub fn select_page_down(&mut self) {
        self.move_selection(20);
    }

    /// Select the newest entry.
    pub fn select_newest(&mut self) {
        if self.list_len() > 0 {
            self.set_selection(0);
        }
    }

    /// Select the oldest entry.
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

    // ── Details scrolling ───────────────────────────────────

    /// First visible line of the details body.
    pub fn details_scroll(&self) -> u16 {
        self.details_scroll
    }

    /// Set the maximum scroll offset of the details body.
    ///
    /// The render calls it, because only the render knows the wrap width and
    /// thus the real line count. The details widget also limits the shown
    /// offset to this value. Thus a terminal resize that makes the content
    /// shorter cannot leave the view after the end.
    pub fn set_details_max_scroll(&self, max: u16) {
        self.details_max_scroll.set(max);
    }

    /// Scroll the details body by `delta` lines. The offset stays in the
    /// content range.
    pub fn scroll_details(&mut self, delta: i32) {
        let max = i32::from(self.details_max_scroll.get());
        let next = (i32::from(self.details_scroll) + delta).clamp(0, max);
        self.details_scroll = next as u16;
    }

    /// Scroll to the top of the details body.
    pub fn scroll_details_to_top(&mut self) {
        self.details_scroll = 0;
    }

    /// Scroll to the bottom of the details body.
    pub fn scroll_details_to_bottom(&mut self) {
        self.details_scroll = self.details_max_scroll.get();
    }

    /// Open the Details sub-tab with a copy of the selected entry. No effect
    /// when there is no selection.
    pub fn open_details(&mut self) {
        match self.sub_tab {
            NetworkSubTab::Requests => {
                if let Some(sel) = self.selected_request
                    && let Some(entry) = display_nth(&self.requests, sel)
                {
                    self.details = Some(DetailView::Request(entry.clone()));
                    self.sub_tab = NetworkSubTab::Details;
                    self.details_scroll = 0;
                }
            }
            NetworkSubTab::Connections => {
                if let Some(sel) = self.selected_connection
                    && let Some(entry) = display_nth(&self.connections, sel)
                {
                    self.details = Some(DetailView::Connection(entry.clone()));
                    self.sub_tab = NetworkSubTab::Details;
                    self.details_scroll = 0;
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

/// Entry at display index `display_idx` (0 = newest = last vec entry).
fn display_nth<T>(vec: &[T], display_idx: usize) -> Option<&T> {
    vec.len()
        .checked_sub(1)
        .and_then(|last| last.checked_sub(display_idx))
        .and_then(|vec_idx| vec.get(vec_idx))
}

/// Update a display-index selection after a new entry is added.
///
/// A selection at 0 (newest) stays at 0, so it follows the newest entry. A
/// selection at `n>0` changes to `n+1`, so it stays on the same entry.
fn on_push_selection(selected: &mut Option<usize>, new_len: usize) {
    match *selected {
        None => {
            if new_len > 0 {
                *selected = Some(0);
            }
        }
        Some(0) => {} // follow the newest entry
        Some(n) => *selected = Some((n + 1).min(new_len.saturating_sub(1))),
    }
}

/// Increment the allowed or the denied counter for one event.
fn bump(allowed: bool, allowed_ctr: &mut u32, denied_ctr: &mut u32) {
    if allowed {
        *allowed_ctr += 1;
    } else {
        *denied_ctr += 1;
    }
}

/// Remove the oldest entries (front of vec) until `vec.len() <= max`.
///
/// If the display-index selection is now out of range, it changes to the
/// last display index.
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

/// Widget that draws the network panel: border, title, sub-tabs, body and
/// footer. The sub-tab modules draw the body.
///
/// The render also stores the click rects in the [`NetworkTab`] for mouse
/// input.
pub struct NetworkWidget<'a> {
    tab: &'a NetworkTab,
    policy: crate::Policy,
    bindings: &'a crate::keys::KeyBindings,
}

impl<'a> NetworkWidget<'a> {
    /// Create a network panel widget.
    /// Args:
    ///  - `tab`: State of the network panel
    ///  - `policy`: Current network policy, for the policy title
    ///  - `bindings`: Key bindings, for the shortcut letter highlights.
    pub fn new(
        tab: &'a NetworkTab,
        policy: crate::Policy,
        bindings: &'a crate::keys::KeyBindings,
    ) -> Self {
        Self {
            tab,
            policy,
            bindings,
        }
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
            Constraint::Length(3), // empty top margin, sub-tab row, separator
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        let details_label = self.tab.details.as_ref().map(|d| match d {
            DetailView::Request(_) => "Request details",
            DetailView::Connection(_) => "Connection details",
        });
        // Highlight the first letter only when the user kept the default
        // `r` / `c` bindings. If not, the cyan letter shows a wrong binding.
        let highlight_letter = |action: crate::keys::Action, label: &str| -> bool {
            let Some(first) = label.chars().next() else {
                return false;
            };
            self.bindings
                .primary(action)
                .is_some_and(|(code, mods)| match code {
                    crossterm::event::KeyCode::Char(c) => {
                        mods.is_empty() && c.eq_ignore_ascii_case(&first)
                    }
                    _ => false,
                })
        };
        let highlight_requests = highlight_letter(crate::keys::Action::SelectRequests, "Requests");
        let highlight_connections =
            highlight_letter(crate::keys::Action::SelectConnections, "Connections");
        let rects = chrome::render_sub_tabs(
            tabs_area,
            self.tab.sub_tab,
            details_label,
            highlight_requests,
            highlight_connections,
            buf,
        );
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
                    let report = |max| self.tab.set_details_max_scroll(max);
                    details::DetailsWidget::new(d, self.tab.details_scroll(), &report)
                        .render(body_area, buf);
                }
            }
        }

        let (allowed, denied) = self.tab.visible_counts();
        footer::render_footer(
            footer_area,
            allowed,
            denied,
            self.tab.sub_tab == NetworkSubTab::Details,
            buf,
        );

        // Draw the dropdown last, so it is on top of the body content.
        if let Some(dropdown) = self.tab.dropdown.as_ref() {
            let rows = chrome::render_policy_dropdown(area, anchor, dropdown.highlighted, buf);
            self.tab.dropdown_rects.set(rows);
        } else {
            self.tab.dropdown_rects.set(Vec::new());
        }
    }
}
