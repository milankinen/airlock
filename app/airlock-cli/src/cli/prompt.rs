//! Interactive prompts on stderr, drawn in place in raw mode.
//!
//! The parts know nothing about airlock: the caller gives the titles,
//! notes, options and checks.
//! - [`screen`]: the terminal (raw mode, the view drawn in place, keys);
//! - [`style`]: the look that the prompts share (styles, marks, lines);
//! - [`choose`]: one option of a vertical radio list;
//! - [`yes_no`]: a question with an inline `« yes · no »` bar;
//! - [`fields`]: `label: value` text inputs, some masked.
//!
//! A prompt is a state that keys change ([`Step`]) and the lines that
//! show it ([`screen::Frame`]); [`run`] draws it until it ends. Every
//! prompt:
//! - refuses to run unless stdin **and** stderr are terminals (it reads
//!   keys from the terminal and draws on stderr);
//! - returns `Ok(None)` when the user presses Esc, so callers can tell
//!   "cancel" from an error;
//! - reports Ctrl+C (a key in raw mode) and a latched interrupt (the
//!   signal handler in [`crate::cli::initialize`]) as
//!   [`PromptError::Interrupted`];
//! - erases its view and restores the terminal on every path.

pub mod choose;
pub mod fields;
pub mod screen;
pub mod style;
pub mod yes_no;

use std::io::IsTerminal;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::cli;
use crate::cli::prompt::screen::{Frame, Input, Screen};

/// Why a prompt produced no answer (Esc is `Ok(None)`, not an error).
#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    /// stdin or stderr is not a terminal.
    #[error("this step needs a terminal (stdin and stderr must be a TTY)")]
    NotInteractive,
    /// Ctrl+C or SIGTERM while the prompt was open.
    #[error("interrupted")]
    Interrupted,
    #[error("prompt failed: {0}")]
    Io(std::io::Error),
}

/// What a key did to a prompt.
pub enum Step<T> {
    /// The prompt stays open.
    Stay,
    /// The prompt ends with the answer.
    Done(T),
    /// Esc (or a cancel option).
    Cancel,
    /// Ctrl-C.
    Interrupt,
}

/// Whether prompts can run: stdin and stderr are both terminals.
pub fn can_prompt() -> bool {
    cli::is_interactive() && std::io::stderr().is_terminal()
}

/// Drop typed-ahead input that nobody read yet (keys pressed while a VM
/// booted), so it does not answer the next prompt. No-op without a TTY.
pub fn flush_input() {
    if std::io::stdin().is_terminal() {
        unsafe { libc::tcflush(0, libc::TCIFLUSH) };
    }
}

/// Whether `key` is Ctrl-C.
pub fn is_interrupt_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
}

/// Apply a text editing `key` to `text`: a typed character goes to its
/// end, Backspace deletes its last one. Returns whether `key` was one of
/// them.
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

/// Show the prompt of `state` (its lines: `frame`, for the columns of
/// text that the terminal has) and apply each key with `key` until it
/// ends. Returns the answer, or `None` on Esc. The view is erased.
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

/// Leave `title` and its `answer` on stderr in place of a closed prompt.
fn report(title: &str, answer: &str) {
    let styles = style::Styles::new();
    eprintln!(
        "{} {} {}",
        styles.bold.apply_to(title),
        styles.dim.apply_to("·"),
        styles.green.apply_to(answer)
    );
}
