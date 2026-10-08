//! Interactive terminal prompts.
//!
//! The prompts know nothing about airlock. The caller gives the titles, notes,
//! options and checks. The available prompts are a single choice from a list, a
//! yes/no question and a form of text inputs. Callers can also make custom
//! prompts.
//!
//! Every prompt:
//!  * fails unless stdin and stderr are terminals
//!  * lets the caller tell a cancel (Esc) from an error
//!  * stops on Ctrl+C or on an earlier interrupt signal
//!  * erases its view and restores the terminal on every path

pub mod choose;
pub mod fields;
pub mod screen;
pub mod style;
pub mod yes_no;

use std::io::IsTerminal;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::cli;
use crate::cli::prompt::screen::{Frame, Input, Screen};

/// Reason why a prompt gave no answer. Esc is `Ok(None)`, not an error.
#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    /// stdin or stderr is not a terminal.
    #[error("this step needs a terminal (stdin and stderr must be a TTY)")]
    NotInteractive,
    /// Ctrl+C or SIGTERM while the prompt was open.
    #[error("interrupted")]
    Interrupted,
    /// Terminal I/O error.
    #[error("prompt failed: {0}")]
    Io(std::io::Error),
}

/// Result of one key press on a prompt.
pub enum Step<T> {
    /// The prompt stays open.
    Stay,
    /// The prompt ends with the answer.
    Done(T),
    /// The user pressed Esc or selected a cancel option.
    Cancel,
    /// The user pressed Ctrl+C.
    Interrupt,
}

/// Return true if prompts can run (stdin and stderr are both terminals).
pub fn can_prompt() -> bool {
    cli::is_interactive() && std::io::stderr().is_terminal()
}

/// Discard unread typed input, for example keys pressed while a VM booted.
/// Thus the input does not answer the next prompt. Does nothing without a TTY.
pub fn flush_input() {
    if std::io::stdin().is_terminal() {
        unsafe { libc::tcflush(0, libc::TCIFLUSH) };
    }
}

/// Return true if `key` is Ctrl+C.
pub fn is_interrupt_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
}

/// Apply a text editing key to `text`.
/// Args:
///  - `text`: Text to edit
///  - `key`: A typed character is added to the end. Backspace deletes the
///    last character.
///
/// Returns:
///   True if `key` was an editing key.
pub fn edit_text(text: &mut String, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Backspace => {
            text.pop();
            true
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            text.push(c);
            true
        }
        _ => false,
    }
}

/// Show a prompt and handle keys until the prompt ends.
/// Args:
///  - `state`: Prompt state
///  - `frame`: Makes the lines of the view from the state and the terminal
///    width in columns
///  - `key`: Applies a key press to the state and returns the next [`Step`]
///
/// Returns:
///   The answer, `None` on Esc, or error. The view is erased at the end.
pub fn run<S, T>(
    state: &mut S,
    frame: impl Fn(&S, usize) -> Frame,
    mut key: impl FnMut(&mut S, KeyEvent) -> Step<T>,
) -> Result<Option<T>, PromptError> {
    if !can_prompt() {
        return Err(PromptError::NotInteractive);
    }
    if cli::is_interrupted() {
        return Err(PromptError::Interrupted);
    }
    let mut screen = Screen::open()?;
    let answer = loop {
        if let Err(e) = screen.draw(|room| frame(state, room)) {
            break Err(e);
        }
        let step = match screen::read_input() {
            Ok(Input::Key(pressed)) => key(state, pressed),
            Ok(Input::Resized) => Step::Stay,
            Ok(Input::Interrupted) => Step::Interrupt,
            Err(e) => break Err(e),
        };
        match step {
            Step::Stay => {}
            Step::Done(answer) => break Ok(Some(answer)),
            Step::Cancel => break Ok(None),
            Step::Interrupt => break Err(PromptError::Interrupted),
        }
    };
    let closed = screen.close();
    let answer = answer?;
    closed?;
    Ok(answer)
}

/// Print `title` and its `answer` on stderr in place of a closed prompt.
fn report(title: &str, answer: &str) {
    let styles = style::Styles::new();
    eprintln!(
        "{} {} {}",
        styles.bold.apply_to(title),
        styles.dim.apply_to("·"),
        styles.green.apply_to(answer)
    );
}
