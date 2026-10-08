//! Early end of `airlock start`.
//!
//! Lets a step of `airlock start` end the run early, with a reason and an exit
//! code.

use std::fmt;

use crate::cli;
use crate::cli::prompt::PromptError;
use crate::project::EnvError;

/// The reason why a step ended the run.
#[derive(Debug)]
pub enum Exit {
    /// Process exit code. The step already reported the reason.
    Code(i32),
    /// A failure that the caller must report.
    Failed(anyhow::Error),
}

impl Exit {
    /// Exit after Ctrl+C (128 + SIGINT).
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

    /// Report that the user cancelled a question. Exit code 0, or 130 after
    /// Ctrl+C.
    pub fn aborted() -> Self {
        if cli::is_interrupted() {
            return Exit::INTERRUPTED;
        }
        cli::error!("Aborted.");
        Exit::Code(0)
    }

    /// Convert the result of the whole run to an exit code or the failure.
    pub fn into_result(result: Result<i32, Exit>) -> anyhow::Result<i32> {
        match result {
            Ok(code) | Err(Exit::Code(code)) => Ok(code),
            Err(Exit::Failed(e)) => Err(e),
        }
    }
}

/// Convert a failure. An `[env]` problem is a configuration error, also
/// when it comes from deep inside the project setup.
impl From<anyhow::Error> for Exit {
    fn from(e: anyhow::Error) -> Self {
        if e.downcast_ref::<EnvError>().is_some() {
            return Exit::config(e);
        }
        Exit::Failed(e)
    }
}

/// Convert a failed question. Reports the error, except for Ctrl+C.
impl From<PromptError> for Exit {
    fn from(e: PromptError) -> Self {
        match e {
            PromptError::Interrupted => Exit::INTERRUPTED,
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
        };
        let wrapped = Err::<(), _>(env_error).context("open project").unwrap_err();
        assert!(matches!(Exit::from(wrapped), Exit::Code(2)));
        assert!(matches!(
            Exit::from(anyhow::anyhow!("disk full")),
            Exit::Failed(_)
        ));
    }
}
