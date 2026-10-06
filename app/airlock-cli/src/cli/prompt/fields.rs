//! Text inputs, one per row: `<label>: <text>`, with the text cursor
//! after the text of the focused row.
//!
//! The view: an optional title (bold) with the rows 2 columns in below
//! it, the rows (the focused label in cyan; a secret text as one `•`
//! per character), the error of the last check in red under its row,
//! and the keys (gray). Keys: characters go to the focused text,
//! Backspace deletes its last one, Enter goes to the next row and on the
//! last one checks the texts (the caller's check), Esc cancels, Ctrl-C
//! interrupts. A failed check keeps the view open, with the focus and
//! the error on the row that it names; a secret text there is cleared
//! (it cannot be edited by sight). The view is erased at the end.

use crossterm::event::{KeyCode, KeyEvent};

use crate::cli::prompt::screen::Frame;
use crate::cli::prompt::style::{self, Line, Styles, Tone};
use crate::cli::prompt::{self, PromptError, Step};

/// The mask of a character of a secret text.
const MASK: &str = "•";

/// A row of the form.
pub struct Field<'a> {
    pub label: &'a str,
    /// Whether the text shows masked.
    pub secret: bool,
}

/// The form.
pub struct Fields<'a> {
    /// The bold line above the rows, if any.
    pub title: Option<&'a str>,
    pub rows: &'a [Field<'a>],
    /// The keys, shown last (gray).
    pub keys: &'a str,
}

/// Why the check rejected the texts.
pub struct Invalid {
    /// The row with the error.
    pub field: usize,
    pub message: String,
}

impl Fields<'_> {
    /// Ask for the texts, one per row: they pass `check`, or `None` on
    /// Esc.
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

    /// The lines of the form in `state` for `room` columns of text.
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

/// The texts, the focused row, and the error of the last check (its
/// row and message) until a text changes.
struct State {
    texts: Vec<String>,
    focus: usize,
    error: Option<(usize, String)>,
}

impl State {
    /// Apply `key`; `Done` asks for the check.
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

    /// Show `invalid` on its row, which gets the focus; a secret text
    /// there is cleared.
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
