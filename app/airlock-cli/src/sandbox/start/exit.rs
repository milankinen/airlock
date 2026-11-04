//! Early end of `airlock start`.
//!
//! Lets a step of `airlock start` end the run early, with a reason and an exit
//! code. All user aborts of a question end the run with the same code.

use std::fmt;

use crate::cli;
use crate::cli::prompt::{self, PromptError};
use crate::packs::approval::ApprovalAborted;
use crate::sandbox::EnvError;

/// The reason why a step ended the run.
#[derive(Debug)]
pub enum Exit {
    /// Process exit code. The step already reported the reason.
    Code(i32),
    /// A failure that the caller must report.
    Failed(anyhow::Error),
}

impl Exit {
    /// Exit after a user abort: Esc, an abort choice or Ctrl+C (128 +
    /// SIGINT).
    pub const ABORTED: Exit = Exit::Code(130);

    /// Report a configuration error (exit code 2).
    pub fn config(e: impl fmt::Display) -> Self {
        cli::error!("{}", crate::messages::start::config_error(e));
        Exit::Code(2)
    }

    /// Report an error of the config resolve. An aborted pack approval or
    /// prompt is an abort, and a failed question is a failed question (see
    /// [`From<PromptError>`]). All other errors are configuration errors.
    pub fn resolve(e: anyhow::Error) -> Self {
        if e.is::<ApprovalAborted>() {
            return Exit::aborted();
        }
        if let Some(abort) = prompt::abort_of(&e) {
            return Exit::abort(abort);
        }
        match e.downcast::<PromptError>() {
            Ok(e) => Exit::from(e),
            Err(e) => Exit::config(e),
        }
    }

    /// Report an error that ends the run with `code`.
    pub fn error(code: i32, e: impl fmt::Display) -> Self {
        cli::error!("{e:#}");
        Exit::Code(code)
    }

    /// Report that the user aborted a question (exit code 130). Ctrl+C
    /// shows no message.
    pub fn aborted() -> Self {
        if !cli::is_interrupted() {
            cli::error!("{}", crate::messages::cli::ABORTED);
        }
        Exit::ABORTED
    }

    /// Report a user abort of a prompt (exit code 130). Esc shows a
    /// message, Ctrl+C shows none.
    pub fn abort(abort: prompt::Abort) -> Self {
        match abort {
            prompt::Abort::Esc => Exit::aborted(),
            prompt::Abort::Interrupt => Exit::ABORTED,
        }
    }

    /// Convert the result of the whole run to an exit code or the failure.
    pub fn into_result(result: Result<i32, Exit>) -> anyhow::Result<i32> {
        match result {
            Ok(code) | Err(Exit::Code(code)) => Ok(code),
            Err(Exit::Failed(e)) => Err(e),
        }
    }
}

/// Convert a failure. An aborted prompt is an abort. An `[env]` problem is
/// a configuration error, also when it comes from deep inside the project
/// setup.
impl From<anyhow::Error> for Exit {
    fn from(e: anyhow::Error) -> Self {
        if let Some(env) = e.downcast_ref::<EnvError>() {
            return match env.abort {
                Some(abort) => Exit::abort(abort),
                None => Exit::config(e),
            };
        }
        if let Some(abort) = prompt::abort_of(&e) {
            return Exit::abort(abort);
        }
        Exit::Failed(e)
    }
}

/// Convert an `[env]` problem. An aborted vault passphrase prompt is an
/// abort. All other problems are configuration errors.
impl From<EnvError> for Exit {
    fn from(e: EnvError) -> Self {
        match e.abort {
            Some(abort) => Exit::abort(abort),
            None => Exit::config(e),
        }
    }
}

/// Convert a failed question. Reports the error, except for Ctrl+C.
impl From<PromptError> for Exit {
    fn from(e: PromptError) -> Self {
        match e {
            PromptError::Interrupted => Exit::ABORTED,
            PromptError::NotInteractive => Exit::error(2, e),
            PromptError::Io(_) => Exit::error(1, e),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the exit codes of failures.

    use anyhow::Context;

    use super::*;

    /// Test that an `[env]` problem gives exit code 2 also when the project
    /// setup returns it inside a general failure with added context. Other
    /// failures stay failures (exit code 1).
    ///   1. Wrap an `[env]` problem in a failure with context
    ///   2. Check that it converts to exit code 2
    ///   3. Check that another failure converts to a failure
    #[test]
    fn env_error_inside_failure_exits_with_config_error_code() {
        let env_error = EnvError {
            name: "TOKEN".into(),
            reason: "host variable is not set".into(),
            abort: None,
        };
        let wrapped = Err::<(), _>(env_error).context("open project").unwrap_err();
        assert!(matches!(Exit::from(wrapped), Exit::Code(2)));
        assert!(matches!(
            Exit::from(anyhow::anyhow!("disk full")),
            Exit::Failed(_)
        ));
    }
}
