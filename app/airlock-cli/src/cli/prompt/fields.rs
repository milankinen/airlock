//! Text input prompt.
//!
//! Shows a form of `label: value` text inputs. Inputs can be masked, for example
//! for passwords. A check from the caller validates the values.

use crossterm::event::{KeyCode, KeyEvent};

use crate::cli::prompt::screen::Frame;
use crate::cli::prompt::style::{self, Line, Styles, Tone};
use crate::cli::prompt::{self, PromptError, Step};

/// Mask that replaces each character of a secret text.
const MASK: &str = "•";

/// One row of the form.
pub struct Field<'a> {
    /// Text before the `: ` and the input.
    pub label: &'a str,
    /// If true, the text shows as one `•` per character.
    pub secret: bool,
}

/// A form of text inputs, one per row: `<label>: <text>`.
///
/// The view shows:
///  * An optional title (bold). Then the rows are indented by 2 columns.
///  * The rows. The focused label is cyan and has the text cursor after its
///    text.
///  * The error of the last check in red, below its row
///  * The key help (gray)
///
/// Keys: characters go to the focused text. Backspace deletes the last
/// character. Enter moves to the next row. On the last row, Enter runs the
/// caller's check. Esc cancels. Ctrl+C interrupts.
///
/// If the check fails, the view stays open. The focus and the error go to
/// the row that the error names. A secret text in that row is cleared,
/// because the user cannot see it to edit it. The view is erased at the end.
pub struct Fields<'a> {
    /// The bold line above the rows, if any.
    pub title: Option<&'a str>,
    /// Rows of the form.
    pub rows: &'a [Field<'a>],
    /// Key help, shown last (gray).
    pub keys: &'a str,
}

/// Check failure: the reason why the check rejected the texts.
pub struct Invalid {
    /// Index of the row with the error.
    pub field: usize,
    /// Error text to show below the row.
    pub message: String,
}

impl Fields<'_> {
    /// Ask for the texts, one per row.
    /// Args:
    ///  - `check`: Validates the texts. On error, the user can edit them again.
    ///
    /// Returns:
    ///   Texts that passed `check`, `None` on Esc, or error.
    pub fn ask(
        &self,
        mut check: impl FnMut(&[String]) -> Result<(), Invalid>,
    ) -> Result<Option<Vec<String>>, PromptError> {
        let mut state = State {
            texts: vec![String::new(); self.rows.len()],
            focus: 0,
            error: None,
        };
        prompt::run(
            &mut state,
            |state, room| self.frame(state, room),
            |state, key| match state.key(key) {
                Step::Done(()) => match check(&state.texts) {
                    Ok(()) => Step::Done(std::mem::take(&mut state.texts)),
                    Err(invalid) => {
                        state.reject(invalid, self.rows);
                        Step::Stay
                    }
                },
                Step::Stay => Step::Stay,
                Step::Cancel => Step::Cancel,
                Step::Interrupt => Step::Interrupt,
            },
        )
    }

    /// Return the lines of the form in `state`, for `room` columns of text.
    fn frame(&self, state: &State, room: usize) -> Frame {
        let styles = Styles::new();
        let mut lines = Vec::new();
        let mut indent = 0;
        if let Some(title) = self.title {
            lines.extend(style::indented(title, 0, room, &styles.bold));
            indent = 2;
        }
        let mut focus = 0..0;
        let mut cursor = None;
        for (i, (field, text)) in self.rows.iter().zip(&state.texts).enumerate() {
            let focused = i == state.focus;
            let first = lines.len();
            let mut line = Line::default();
            line.lead(indent, focused, &styles);
            line.mark_indent();
            line.push(
                &format!("{}: ", field.label),
                styles.label(focused, Tone::Plain),
            );
            if field.secret {
                line.push(&MASK.repeat(text.chars().count()), &styles.plain);
            } else {
                line.push(text, &styles.plain);
            }
            if focused {
                cursor = Some((first, line.width));
            }
            let label_column = line.indent;
            lines.push(line.text);
            if let Some((_, error)) = state.error.as_ref().filter(|(row, _)| *row == i) {
                for error in error.lines() {
                    lines.extend(style::indented(error, label_column, room, &styles.red));
                }
            }
            if focused {
                let start = if i == 0 { 0 } else { first };
                focus = start..lines.len();
            }
        }
        lines.push(styles.dim.apply_to(self.keys).to_string());
        Frame {
            lines,
            focus,
            cursor,
        }
    }
}

/// The texts, the focused row and the error of the last check. The error
/// (row and message) stays until a text changes.
struct State {
    texts: Vec<String>,
    focus: usize,
    error: Option<(usize, String)>,
}

impl State {
    /// Apply `key`. `Done` means that the caller must run the check.
    fn key(&mut self, key: KeyEvent) -> Step<()> {
        if prompt::is_interrupt_key(key) {
            return Step::Interrupt;
        }
        if let Some(text) = self.texts.get_mut(self.focus)
            && prompt::edit_text(text, key)
        {
            self.error = None;
            return Step::Stay;
        }
        match key.code {
            KeyCode::Enter if self.focus + 1 < self.texts.len() => self.focus += 1,
            KeyCode::Enter => return Step::Done(()),
            KeyCode::Esc => return Step::Cancel,
            _ => {}
        }
        Step::Stay
    }

    /// Show `invalid` on its row and move the focus there. Clear a secret
    /// text in that row.
    fn reject(&mut self, invalid: Invalid, fields: &[Field<'_>]) {
        let row = invalid.field.min(self.texts.len().saturating_sub(1));
        if fields.get(row).is_some_and(|field| field.secret)
            && let Some(text) = self.texts.get_mut(row)
        {
            text.clear();
        }
        self.focus = row;
        self.error = Some((row, invalid.message));
    }
}
