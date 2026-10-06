//! How a step of `airlock start` ends the run early.

use std::fmt;

use crate::cli;
use crate::cli::prompt::PromptError;

/// A step that ended the run: the process exit code (the step reported
/// why), or a failure that the caller reports.
#[derive(Debug)]
pub enum Exit {
    Code(i32),
    Failed(anyhow::Error),
}

impl Exit {
    /// Ctrl+C (128 + SIGINT).
    pub const INTERRUPTED: Exit = Exit::Code(130);

    /// Report a configuration error (exit code 2).
    pub fn config(e: impl fmt::Display) -> Self {
        cli::error!("Config error: {e:#}");
        Exit::Code(2)
    }

    /// Report an error that ends the run with `code`.
    pub fn error(code: i32, e: impl fmt::Display) -> Self {
        cli::error!("{e:#}");
        Exit::Code(code)
    }

    /// The user cancelled a question: exit code 0, or 130 after Ctrl+C.
    pub fn aborted() -> Self {
        if cli::is_interrupted() {
            return Exit::INTERRUPTED;
        }
        cli::error!("Aborted.");
        Exit::Code(0)
    }

    /// The result of the whole run: an exit code, or the failure.
    pub fn into_result(result: Result<i32, Exit>) -> anyhow::Result<i32> {
        match result {
            Ok(code) | Err(Exit::Code(code)) => Ok(code),
            Err(Exit::Failed(e)) => Err(e),
        }
    }
}

impl From<anyhow::Error> for Exit {
    fn from(e: anyhow::Error) -> Self {
        Exit::Failed(e)
    }
}

/// A failed question: reported, except Ctrl+C.
impl From<PromptError> for Exit {
    fn from(e: PromptError) -> Self {
        match e {
            PromptError::Interrupted => Exit::INTERRUPTED,
            PromptError::NotInteractive => Exit::error(2, e),
            PromptError::Io(_) => Exit::error(1, e),
        }
    }
}
