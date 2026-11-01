//! Approval dialog of the TUI.
//!
//! Shows a request from the host that the user must allow or deny, for
//! example temporary network access. The host uses the dialog only when
//! the system notification and dialog cannot show. The dialog shows on top
//! of all tabs and takes all input until the user decides. It shows a
//! short form first, with the same text as the desktop notification. The
//! user can expand it to show all details of the request, as in the
//! desktop details window. The dialog goes away when the host stops the wait.

mod render;

use std::cell::Cell;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
pub(crate) use render::render_dialog;
use tokio::sync::oneshot;

/// Request that the user must allow or deny. All text is clean: the host
/// removed control characters and escape sequences.
#[derive(Debug)]
pub struct ApprovalRequest {
    /// Title line.
    pub title: String,
    /// Short body.
    pub body: String,
    /// All details of the request, for the expanded form.
    pub details: Vec<ApprovalDetail>,
    /// Gets `true` when the user allows the request, and `false` when the
    /// user denies it. The dialog goes away with no answer when the host
    /// drops the receiver.
    pub reply: oneshot::Sender<bool>,
}

/// One labeled value in the details of a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalDetail {
    /// Label of the value.
    pub label: String,
    /// Value. It can have more lines.
    pub value: String,
    /// True if the value is code or data, for example a command or TOML.
    pub mono: bool,
    /// Lines of a change, if the value is a diff.
    pub diff: Option<Vec<ApprovalDiffLine>>,
}

/// One line of a diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalDiffLine {
    /// Kind of the line.
    pub kind: ApprovalDiffKind,
    /// Text of the line, with no mark.
    pub text: String,
}

/// Kind of one diff line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDiffKind {
    /// The line is new.
    Add,
    /// The line goes away.
    Remove,
    /// The line does not change.
    Same,
}

/// Button of the approval dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Button {
    /// Allow the request.
    Accept,
    /// Deny the request.
    Deny,
    /// Show or hide the details.
    Toggle,
}

/// Columns to move for one horizontal scroll step.
const STEP_X: u16 = 4;
/// Rows to move for one mouse wheel step.
const WHEEL_Y: u16 = 3;
/// Time after a dialog shows in which it ignores answers. A key or click
/// that the user aimed at the guest or at the previous dialog must not
/// answer the new dialog.
const GRACE: Duration = Duration::from_millis(300);
/// Grace time of a dialog that shows right after the user answered the
/// previous one. It is longer than the first auto-repeat delay of a held
/// key (250 to 660 ms), so a held answer key never answers the next one.
const ANSWER_GRACE: Duration = Duration::from_secs(1);
/// Smallest time between an answer key and the key before it. A held key
/// sends a fast series of presses, and these must not answer.
const KEY_GAP: Duration = Duration::from_millis(150);

/// Queue of approval requests and the view state of the visible dialog.
///
/// Only the first request shows. The next request shows when the user
/// answers the first one, or when the host stops the wait for it.
pub(crate) struct Approvals {
    queue: VecDeque<ApprovalRequest>,
    /// True if the dialog shows the details.
    expanded: bool,
    /// First visible row of the content.
    scroll_y: u16,
    /// First visible column of the content.
    scroll_x: u16,
    /// Time when the first request in the queue started to show.
    shown_at: Instant,
    /// Time after `shown_at` in which the dialog ignores answers. Tests
    /// change it.
    pub(crate) grace: Duration,
    /// Grace time of a dialog that shows right after an answer. Tests
    /// change it.
    pub(crate) answer_grace: Duration,
    /// True if the visible dialog showed right after an answer. Then it
    /// uses `answer_grace`, else `grace`.
    after_answer: bool,
    /// Time of the last key event that the dialog got, or `None` before
    /// the first one.
    last_key_at: Option<Instant>,
    /// Smallest time between an answer key and the key before it. Tests
    /// change it.
    pub(crate) key_gap: Duration,
    /// True if the last draw of the visible request showed its content.
    /// The user must see the content before an accept or a toggle.
    seen: Cell<bool>,
    /// True after a mouse press that the dialog got, until the release.
    /// The release must not go to the guest when the press closed the
    /// dialog.
    mouse_held: bool,
    /// Data from the last draw. The input handlers use it to limit the
    /// scroll and to find the click targets.
    layout: Cell<DialogLayout>,
}

impl Default for Approvals {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            expanded: false,
            scroll_y: 0,
            scroll_x: 0,
            shown_at: Instant::now(),
            grace: GRACE,
            answer_grace: ANSWER_GRACE,
            after_answer: false,
            last_key_at: None,
            key_gap: KEY_GAP,
            seen: Cell::new(false),
            mouse_held: false,
            layout: Cell::new(DialogLayout::default()),
        }
    }
}

/// Data from the last draw of the dialog.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DialogLayout {
    /// Largest useful vertical scroll offset.
    pub max_y: u16,
    /// Largest useful horizontal scroll offset.
    pub max_x: u16,
    /// Visible rows of the content. Zero if the content did not show, for
    /// example on a very small terminal.
    pub page: u16,
    /// Click rect of the accept button.
    pub accept: Rect,
    /// Click rect of the deny button.
    pub deny: Rect,
    /// Click rect of the button that shows or hides the details.
    pub toggle: Rect,
    /// Rect of the visible content. A click on it in the short form shows
    /// the details.
    pub content: Rect,
}

impl Approvals {
    /// Add a request at the end of the queue.
    pub fn push(&mut self, request: ApprovalRequest) {
        if self.queue.is_empty() {
            self.reset_view(false);
        }
        self.queue.push_back(request);
    }

    /// Remove the requests that the host does not wait for. Call it before
    /// each draw.
    pub fn drop_closed(&mut self) {
        let front_closed = self.queue.front().is_some_and(|r| r.reply.is_closed());
        self.queue.retain(|r| !r.reply.is_closed());
        if front_closed {
            self.reset_view(false);
        }
    }

    /// Request that shows now, or `None` if there is no dialog.
    pub fn active(&self) -> Option<&ApprovalRequest> {
        self.queue.front()
    }

    /// True if a dialog shows. Then the dialog gets all input.
    pub fn is_open(&self) -> bool {
        !self.queue.is_empty()
    }

    /// True if the dialog shows the details.
    pub fn expanded(&self) -> bool {
        self.expanded
    }

    /// Scroll offset of the content as (row, column).
    pub fn scroll(&self) -> (u16, u16) {
        (self.scroll_y, self.scroll_x)
    }

    /// Keep the data of the last draw for the input handlers. A layout
    /// with no content rows marks the request as not seen.
    pub fn set_layout(&self, layout: DialogLayout) {
        self.seen.set(layout.page > 0);
        self.layout.set(layout);
    }

    /// Apply a key to the dialog. Call it only when a dialog shows.
    pub fn handle_key(&mut self, key: KeyEvent) {
        // All key events count for the gap, also the ignored ones. Thus
        // the fast presses of a held key never answer.
        let now = Instant::now();
        let after_gap = self
            .last_key_at
            .is_none_or(|t| now.duration_since(t) >= self.key_gap);
        self.last_key_at = Some(now);
        // Ignore keys with Ctrl, Alt or Super, so that no shortcut of the
        // TUI or the guest answers the request by accident. Shift is
        // permitted for upper case letters.
        if key.kind == KeyEventKind::Release || !(key.modifiers - KeyModifiers::SHIFT).is_empty() {
            return;
        }
        // A held key must not answer the request or toggle the form. Only
        // the scroll keys repeat.
        let first = key.kind == KeyEventKind::Press;
        let answer = first && after_gap;
        let page = self.layout.get().page.saturating_sub(1).max(1);
        match key.code {
            KeyCode::Char(c) => match c.to_ascii_lowercase() {
                'a' if answer => self.press(Button::Accept),
                'd' if answer => self.press(Button::Deny),
                's' if first => self.press(Button::Toggle),
                'k' => self.scroll_by(-1, 0),
                'j' => self.scroll_by(1, 0),
                'h' => self.scroll_by(0, -i32::from(STEP_X)),
                'l' => self.scroll_by(0, i32::from(STEP_X)),
                _ => {}
            },
            KeyCode::Up => self.scroll_by(-1, 0),
            KeyCode::Down => self.scroll_by(1, 0),
            KeyCode::Left => self.scroll_by(0, -i32::from(STEP_X)),
            KeyCode::Right => self.scroll_by(0, i32::from(STEP_X)),
            KeyCode::PageUp => self.scroll_by(-i32::from(page), 0),
            KeyCode::PageDown => self.scroll_by(i32::from(page), 0),
            KeyCode::Home => (self.scroll_y, self.scroll_x) = (0, 0),
            KeyCode::End => self.scroll_y = self.layout.get().max_y,
            // Esc never answers the request. It only hides the details.
            KeyCode::Esc if first && self.expanded => self.press(Button::Toggle),
            _ => {}
        }
    }

    /// Apply a mouse event to the dialog. Call it only when a dialog shows.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
        let wheel_y = i32::from(WHEEL_Y);
        let step_x = i32::from(STEP_X);
        match mouse.kind {
            MouseEventKind::Down(_) => {
                self.mouse_held = true;
                if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
                    return;
                }
                let layout = self.layout.get();
                let at = Position::new(mouse.column, mouse.row);
                for (button, rect) in [
                    (Button::Accept, layout.accept),
                    (Button::Deny, layout.deny),
                    (Button::Toggle, layout.toggle),
                ] {
                    if rect.contains(at) {
                        self.press(button);
                        return;
                    }
                }
                // A click on the short form shows the details, as a click
                // on a desktop notification opens its details window.
                if !self.expanded && layout.content.contains(at) {
                    self.press(Button::Toggle);
                }
            }
            MouseEventKind::Up(_) => self.mouse_held = false,
            // Many terminals send Shift+wheel for a horizontal scroll.
            MouseEventKind::ScrollUp if shift => self.scroll_by(0, -step_x),
            MouseEventKind::ScrollDown if shift => self.scroll_by(0, step_x),
            MouseEventKind::ScrollUp => self.scroll_by(-wheel_y, 0),
            MouseEventKind::ScrollDown => self.scroll_by(wheel_y, 0),
            MouseEventKind::ScrollLeft => self.scroll_by(0, -step_x),
            MouseEventKind::ScrollRight => self.scroll_by(0, step_x),
            _ => {}
        }
    }

    /// True if a mouse event that comes after the dialog closed is the end
    /// of a click on the dialog. The caller must then discard the event.
    /// Call it only when no dialog shows.
    pub fn is_click_tail(&mut self, mouse: MouseEvent) -> bool {
        if !self.mouse_held {
            return false;
        }
        match mouse.kind {
            MouseEventKind::Drag(_) => true,
            MouseEventKind::Up(_) => {
                self.mouse_held = false;
                true
            }
            _ => {
                self.mouse_held = false;
                false
            }
        }
    }

    /// Do the action of `button`.
    fn press(&mut self, button: Button) {
        // Deny is always permitted, so that the user can close a dialog
        // that does not fit on the screen.
        let seen = self.seen.get();
        let grace = if self.after_answer {
            self.answer_grace
        } else {
            self.grace
        };
        let in_grace = self.shown_at.elapsed() < grace;
        match button {
            Button::Accept if seen && !in_grace => self.answer(true),
            Button::Deny if !in_grace => self.answer(false),
            Button::Toggle if seen => {
                self.expanded = !self.expanded;
                self.scroll_y = 0;
                self.scroll_x = 0;
                self.clear_scroll_limits();
            }
            _ => {}
        }
    }

    /// Send the answer for the visible request and show the next one.
    fn answer(&mut self, allow: bool) {
        if let Some(request) = self.queue.pop_front() {
            // The host can stop the wait at any time. Then no one gets the
            // answer, and that is correct.
            let _ = request.reply.send(allow);
        }
        self.reset_view(true);
    }

    /// Show the next dialog in the short form, at the top, and start its
    /// grace time.
    /// Args:
    ///  - `after_answer`: True if the user answered the previous dialog.
    ///    Then the next dialog gets the longer grace time.
    fn reset_view(&mut self, after_answer: bool) {
        self.after_answer = after_answer;
        self.expanded = false;
        self.scroll_y = 0;
        self.scroll_x = 0;
        self.shown_at = Instant::now();
        self.seen.set(false);
        self.clear_scroll_limits();
    }

    /// Move the time of the dialog `by` into the past, as if that time went
    /// by with no input.
    #[cfg(test)]
    pub fn age(&mut self, by: Duration) {
        self.shown_at -= by;
        if let Some(t) = self.last_key_at.as_mut() {
            *t -= by;
        }
    }

    /// Remove the scroll limits of the last draw. They are for different
    /// content. Keep the buttons, so that the next keys and clicks in the
    /// same batch of events still work. The next draw sets new limits.
    fn clear_scroll_limits(&self) {
        let mut layout = self.layout.get();
        layout.max_y = 0;
        layout.max_x = 0;
        self.layout.set(layout);
    }

    /// Move the content by `dy` rows and `dx` columns, in the limits of the
    /// last draw.
    fn scroll_by(&mut self, dy: i32, dx: i32) {
        let layout = self.layout.get();
        // Limit the old offset first. It can be out of the limits after a
        // resize, and then a step back must move the visible content.
        let clamp = |pos: u16, delta: i32, max: u16| -> u16 {
            (i32::from(pos.min(max)) + delta).clamp(0, i32::from(max)) as u16
        };
        self.scroll_y = clamp(self.scroll_y, dy, layout.max_y);
        self.scroll_x = clamp(self.scroll_x, dx, layout.max_x);
    }
}
