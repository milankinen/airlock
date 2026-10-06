//! A yes/no question with an inline bar: `<question>  « yes · no »`.
//!
//! The question is bold; the current answer in the bar is green (without
//! colors: in brackets). The keys (gray) go on the next line. When the
//! question and the bar do not fit on a line, the question wraps and the
//! bar goes below it. Keys: ←/→ change the answer, Enter confirms it,
//! Esc cancels, Ctrl-C interrupts. The question and the answer stay on
//! the terminal.

use console::measure_text_width;
use crossterm::event::{KeyCode, KeyEvent};

use crate::cli::prompt::screen::Frame;
use crate::cli::prompt::style::{self, Line, Styles, Tone};
use crate::cli::prompt::{self, PromptError, Step};

/// The keys of the bar.
const KEYS: &str = "←→ choose · enter confirm";

/// The question.
pub struct YesNo<'a> {
    pub question: &'a str,
    /// The answer that is current at first.
    pub default: bool,
}

impl YesNo<'_> {
    /// Ask the question: the answer, or `None` on Esc.
    pub fn ask(&self) -> Result<Option<bool>, PromptError> {
        let mut yes = self.default;
        let answer = prompt::run(&mut yes, |yes, room| self.frame(*yes, room), key)?;
        if let Some(yes) = answer {
            prompt::report(self.question, label(yes));
        }
        Ok(answer)
    }

    /// The lines of the question with `yes` current, for `room` columns
    /// of text.
    fn frame(&self, yes: bool, room: usize) -> Frame {
        let styles = Styles::new();
        let mut bar = Line::default();
        bar.push("« ", &styles.plain);
        bar.push_option(label(true), yes, Tone::Plain, &styles);
        bar.push(" · ", &styles.plain);
        bar.push_option(label(false), !yes, Tone::Plain, &styles);
        bar.push(" »", &styles.plain);
        let question = styles.bold.apply_to(self.question);
        let mut lines = if measure_text_width(self.question) + 2 + bar.width <= room {
            vec![format!("{question}  {}", bar.text)]
        } else {
            let mut lines = style::indented(self.question, 0, room, &styles.bold);
            lines.push(bar.text);
            lines
        };
        lines.push(styles.dim.apply_to(KEYS).to_string());
        Frame {
            focus: 0..lines.len(),
            lines,
            cursor: None,
        }
    }
}

/// The text of an answer.
fn label(yes: bool) -> &'static str {
    if yes { "yes" } else { "no" }
}

fn key(yes: &mut bool, key: KeyEvent) -> Step<bool> {
    if prompt::is_interrupt_key(key) {
        return Step::Interrupt;
    }
    match key.code {
        KeyCode::Left => *yes = true,
        KeyCode::Right => *yes = false,
        KeyCode::Enter => return Step::Done(*yes),
        KeyCode::Esc => return Step::Cancel,
        _ => {}
    }
    Step::Stay
}
