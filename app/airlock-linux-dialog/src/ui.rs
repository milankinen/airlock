//! Look and input handling of the dialog.
//!
//! Draws the text of the request, a scroll bar, a countdown and two
//! buttons. Turns clicks and keys into scroll moves or an answer. The deny
//! button is the first button. No key selects allow, and a click on a
//! button counts only after a short time, so that a click meant for another
//! window cannot allow the request.

use std::time::Duration;

use crate::layout::{self, Line, Scroll, Style};
use crate::protocol::{Answer, Request};
use crate::render::{Canvas, Color, Font, Rect};

const BACKGROUND: Color = 0xf7f7f7;
const TEXT: Color = 0x202020;
const LABEL: Color = 0x0a4a8a;
const ADDED: Color = 0x1a7f37;
const ADDED_BACK: Color = 0xdafbe1;
const REMOVED: Color = 0xa40e26;
const REMOVED_BACK: Color = 0xffebe9;
const KEPT: Color = 0x606060;
const BUTTON: Color = 0xe4e4e4;
const BUTTON_BORDER: Color = 0x808080;
const BAR: Color = 0xc0c0c0;
const MUTED: Color = 0x606060;

/// Time after the window shows in which clicks on buttons have no effect.
pub(crate) const CLICK_GUARD: Duration = Duration::from_millis(600);

/// Key that the dialog uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    Escape,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

/// State of the dialog.
pub(crate) struct Ui {
    request: Request,
    font: Font,
    lines: Vec<Line>,
    scroll: Scroll,
    width: i32,
    height: i32,
}

impl Ui {
    /// Make the dialog for a request.
    /// Args:
    ///  - `request`: Request to show
    ///  - `scale`: Scale of the screen, 1.0 for 96 dpi
    pub fn new(request: Request, scale: f32) -> Self {
        let font = Font::new((14.0 * scale).round());
        let mut ui = Self {
            request,
            font,
            lines: Vec::new(),
            scroll: Scroll::default(),
            width: 0,
            height: 0,
        };
        let (w, h) = ui.preferred_size();
        ui.resize(w, h);
        ui
    }

    /// Size that shows 90 columns and 26 lines.
    pub fn preferred_size(&self) -> (i32, i32) {
        let w = self.pad() * 2 + self.bar_w() + 90 * self.font.cell_w;
        let h = self.footer_h() + self.pad() * 2 + 26 * self.font.cell_h;
        (w, h)
    }

    /// Window title.
    pub fn title(&self) -> &str {
        &self.request.title
    }

    /// Smallest size that works.
    pub fn minimum_size(&self) -> (i32, i32) {
        let w = self.pad() * 2 + self.bar_w() + 40 * self.font.cell_w;
        let h = self.footer_h() + self.pad() * 2 + 6 * self.font.cell_h;
        (w, h)
    }

    fn pad(&self) -> i32 {
        self.font.cell_w * 2
    }

    fn bar_w(&self) -> i32 {
        self.font.cell_w
    }

    fn footer_h(&self) -> i32 {
        self.font.cell_h * 3
    }

    /// Set the size of the window and make the lines again.
    pub fn resize(&mut self, width: i32, height: i32) {
        self.width = width;
        self.height = height;
        let cols = ((width - self.pad() * 2 - self.bar_w()) / self.font.cell_w).max(1);
        self.lines = layout::build(&self.request.items, cols as usize);
        let rows = (self.text_area().h / self.font.cell_h).max(1);
        self.scroll.set_size(rows as usize, self.lines.len());
    }

    fn text_area(&self) -> Rect {
        Rect::new(
            self.pad(),
            self.pad(),
            self.width - self.pad() * 2 - self.bar_w(),
            self.height - self.footer_h() - self.pad() * 2,
        )
    }

    /// Boxes of the deny and allow buttons.
    fn buttons(&self) -> (Rect, Rect) {
        let c = &self.font;
        let chars = self
            .request
            .allow
            .chars()
            .count()
            .max(self.request.deny.chars().count());
        let w = (chars as i32 + 6) * c.cell_w;
        let h = c.cell_h * 2;
        let y = self.height - self.footer_h() + (self.footer_h() - h) / 2;
        let allow = Rect {
            x: self.width - self.pad() - w,
            y,
            w,
            h,
        };
        let deny = Rect {
            x: allow.x - c.cell_w * 2 - w,
            ..allow
        };
        (deny, allow)
    }

    /// Draw the whole window.
    /// Args:
    ///  - `left`: Time to the end of the time limit
    pub fn draw(&mut self, canvas: &mut Canvas, left: Duration) {
        canvas.fill(Rect::new(0, 0, self.width, self.height), BACKGROUND);
        let area = self.text_area();
        let rows = self.scroll.visible;
        for (n, line) in self
            .lines
            .iter()
            .skip(self.scroll.offset)
            .take(rows)
            .enumerate()
        {
            let y = area.y + n as i32 * self.font.cell_h;
            let color = match line.style {
                Style::Label => LABEL,
                Style::Text => TEXT,
                Style::Added => {
                    canvas.fill(Rect::new(area.x, y, area.w, self.font.cell_h), ADDED_BACK);
                    ADDED
                }
                Style::Removed => {
                    canvas.fill(Rect::new(area.x, y, area.w, self.font.cell_h), REMOVED_BACK);
                    REMOVED
                }
                Style::Kept => KEPT,
            };
            self.font.draw(canvas, area.x, y, &line.text, color);
        }
        if self.scroll.scrolls() {
            let (y, h) = self.thumb(area);
            let x = area.x + area.w + self.bar_w() / 4;
            canvas.fill(Rect::new(x, y, self.bar_w() / 2, h), BAR);
        }
        let footer = self.height - self.footer_h();
        canvas.fill(Rect::new(0, footer, self.width, 1), BUTTON_BORDER);
        let (deny, allow) = self.buttons();
        for (rect, label) in [(deny, &self.request.deny), (allow, &self.request.allow)] {
            canvas.fill(rect, BUTTON);
            canvas.border(rect, 1, BUTTON_BORDER);
            let x = rect.x + (rect.w - label.chars().count() as i32 * self.font.cell_w) / 2;
            let y = rect.y + (rect.h - self.font.cell_h) / 2;
            self.font.draw(canvas, x, y, label, TEXT);
        }
        // Deny is the default button: it has a thick border.
        canvas.border(deny, 3, LABEL);
        let note = format!("{} {}s", self.request.countdown, left.as_secs() + 1);
        let y = footer + (self.footer_h() - self.font.cell_h) / 2;
        self.font.draw(canvas, self.pad(), y, &note, MUTED);
    }

    /// Top and height of the scroll bar thumb.
    fn thumb(&self, area: Rect) -> (i32, i32) {
        let total = self.scroll.total.max(1) as i32;
        let h = (area.h * self.scroll.visible as i32 / total).max(self.font.cell_h);
        let span = area.h - h;
        let max = (self.scroll.total - self.scroll.visible).max(1) as i32;
        (area.y + span * self.scroll.offset as i32 / max, h)
    }

    /// Handle a click.
    /// Args:
    ///  - `shown`: Time since the window showed
    ///
    /// Returns:
    ///   The answer, if the click selects a button.
    pub fn click(&self, x: i32, y: i32, shown: Duration) -> Option<Answer> {
        if shown < CLICK_GUARD {
            return None;
        }
        let (deny, allow) = self.buttons();
        if deny.contains(x, y) {
            Some(Answer::Deny)
        } else if allow.contains(x, y) {
            Some(Answer::Allow)
        } else {
            None
        }
    }

    /// Scroll with the mouse wheel. A positive `steps` moves down.
    pub fn wheel(&mut self, steps: isize) {
        self.scroll.by(steps * 3);
    }

    /// Handle a key.
    ///
    /// Returns:
    ///   The answer, if the key gives one. Only Escape does, and it denies.
    pub fn key(&mut self, key: Key) -> Option<Answer> {
        let page = self.scroll.visible.saturating_sub(1).max(1) as isize;
        match key {
            Key::Escape => return Some(Answer::Deny),
            Key::Up => self.scroll.by(-1),
            Key::Down => self.scroll.by(1),
            Key::PageUp => self.scroll.by(-page),
            Key::PageDown => self.scroll.by(page),
            Key::Home => self.scroll.home(),
            Key::End => self.scroll.end(),
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Item;

    fn request(lines: usize) -> Request {
        Request {
            title: "airlock".into(),
            items: vec![Item {
                label: "Command".into(),
                value: (0..lines)
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
                diff: None,
            }],
            allow: "Allow".into(),
            deny: "Deny".into(),
            countdown: "Denies in".into(),
            timeout_ms: 30_000,
        }
    }

    fn center(r: Rect) -> (i32, i32) {
        (r.x + r.w / 2, r.y + r.h / 2)
    }

    /// Test that clicks select buttons, and that early clicks do nothing.
    ///   1. Click each button after the guard time, and click the text
    ///   2. Click the allow button before the guard time ends
    #[test]
    fn click_selects_button_only_after_guard_time() {
        let ui = Ui::new(request(3), 1.0);
        let (deny, allow) = ui.buttons();
        let late = CLICK_GUARD + Duration::from_millis(1);
        let (x, y) = center(deny);
        assert_eq!(ui.click(x, y, late), Some(Answer::Deny));
        let (x, y) = center(allow);
        assert_eq!(ui.click(x, y, late), Some(Answer::Allow));
        assert_eq!(ui.click(20, 20, late), None);
        assert_eq!(ui.click(x, y, Duration::from_millis(100)), None);
    }

    /// Test that only Escape gives an answer, and that it denies.
    ///   1. Press every key that the dialog knows
    ///   2. Check that only Escape gives an answer
    #[test]
    fn only_escape_answers_and_it_denies() {
        let mut ui = Ui::new(request(100), 1.0);
        for key in [
            Key::Up,
            Key::Down,
            Key::PageUp,
            Key::PageDown,
            Key::Home,
            Key::End,
        ] {
            assert_eq!(ui.key(key), None);
        }
        assert_eq!(ui.key(Key::Escape), Some(Answer::Deny));
    }

    /// Test that long text scrolls and a resize keeps the position valid.
    ///   1. Show 100 lines and scroll to the end
    ///   2. Make the window taller and check the offset
    #[test]
    fn long_text_scrolls_and_resize_keeps_offset_valid() {
        let mut ui = Ui::new(request(100), 1.0);
        assert!(ui.scroll.scrolls());
        ui.key(Key::End);
        let end = ui.scroll.offset;
        assert!(end > 0);
        let (w, h) = ui.preferred_size();
        ui.resize(w, h * 2);
        assert!(ui.scroll.offset < end);
        assert_eq!(ui.scroll.offset, ui.scroll.total - ui.scroll.visible);
    }

    /// Test that the window draws the text, the buttons and the countdown.
    ///   1. Draw a request into a canvas
    ///   2. Check that the canvas has dark text pixels and button borders
    #[test]
    fn draw_shows_text_and_buttons() {
        let mut ui = Ui::new(request(3), 1.0);
        let (w, h) = ui.preferred_size();
        let mut canvas = Canvas::new(w as usize, h as usize, 0);
        ui.draw(&mut canvas, Duration::from_secs(29));
        let (deny, allow) = ui.buttons();
        let at = |x: i32, y: i32| canvas.pixels[y as usize * canvas.width + x as usize];
        assert_eq!(at(allow.x, allow.y), BUTTON_BORDER);
        assert_eq!(at(deny.x, deny.y), LABEL);
        assert!(canvas.pixels.contains(&LABEL));
        assert!(canvas.pixels.iter().any(|&p| p != BACKGROUND && p != LABEL));
    }
}
