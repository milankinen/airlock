//! Single-choice prompt.
//!
//! Lets the user select one option from a vertical radio list.

use crossterm::event::{KeyCode, KeyEvent};

use crate::cli::prompt::screen::Frame;
use crate::cli::prompt::style::{self, Line, Styles, Tone};
use crate::cli::prompt::{self, PromptError, Step};

/// Key help line of the list.
const KEYS: &str = "↑↓ move · enter confirm · esc cancel";

/// One option of the list.
pub struct Choice<'a> {
    /// Option text.
    pub label: &'a str,
    /// Color of the label. A [`Tone::Danger`] label is red.
    pub tone: Tone,
}

/// A prompt that selects one option from a vertical radio list.
///
/// The view shows:
///  * The title (bold)
///  * The notes (gray, indented by 2 columns)
///  * One option per line. The focused option has a green `◉` and a cyan
///    label.
///  * An empty line and the key help (gray)
///
/// Keys: ↑/↓ move the focus (and wrap at the ends). Enter selects the focused
/// option. Esc cancels. Ctrl+C interrupts.
pub struct Choose<'a> {
    /// Question text (bold). Can have many lines.
    pub title: &'a str,
    /// Gray lines between the title and the options.
    pub notes: &'a [&'a str],
    /// Options of the list.
    pub choices: &'a [Choice<'a>],
    /// Index of the option that has the focus at the start.
    pub default: usize,
    /// If true, the title and the selected option stay on the terminal.
    pub report: bool,
}

impl Choose<'_> {
    /// Ask the question.
    /// Returns:
    ///   Index of the selected option, `None` on Esc, or error.
    pub fn ask(&self) -> Result<Option<usize>, PromptError> {
        let mut state = State {
            focus: self.default.min(self.choices.len().saturating_sub(1)),
            len: self.choices.len(),
        };
        let picked = prompt::run(
            &mut state,
            |state, room| self.frame(state, room),
            State::key,
        )?;
        if let Some(i) = picked
            && self.report
        {
            prompt::report(self.title, self.choices[i].label);
        }
        Ok(picked)
    }

    /// Return the lines of the list in `state`, for `room` columns of text.
    fn frame(&self, state: &State, room: usize) -> Frame {
        let styles = Styles::new();
        let mut lines: Vec<String> = self
            .title
            .lines()
            .flat_map(|line| style::indented(line, 0, room, &styles.bold))
            .collect();
        for note in self.notes {
            lines.extend(style::indented(note, 2, room, &styles.dim));
        }
        let mut focus = 0..0;
        for (i, choice) in self.choices.iter().enumerate() {
            let focused = i == state.focus;
            let mut line = Line::default();
            line.lead(2, focused, &styles);
            line.push(style::radio(focused), styles.mark(focused));
            line.push(" ", &styles.plain);
            line.push(choice.label, styles.label(focused, choice.tone));
            if focused {
                let start = if i == 0 { 0 } else { lines.len() };
                focus = start..lines.len() + 1;
            }
            lines.push(line.text);
        }
        lines.push(String::new());
        lines.push(styles.dim.apply_to(KEYS).to_string());
        Frame {
            lines,
            focus,
            cursor: None,
        }
    }
}

/// The focused option in a list of `len` options.
struct State {
    focus: usize,
    len: usize,
}

impl State {
    fn key(&mut self, key: KeyEvent) -> Step<usize> {
        if prompt::is_interrupt_key(key) {
            return Step::Interrupt;
        }
        let len = self.len.max(1);
        match key.code {
            KeyCode::Up => self.focus = (self.focus + len - 1) % len,
            KeyCode::Down => self.focus = (self.focus + 1) % len,
            KeyCode::Enter if self.len > 0 => return Step::Done(self.focus),
            KeyCode::Esc => return Step::Cancel,
            _ => {}
        }
        Step::Stay
    }
}
