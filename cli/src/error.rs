#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Expected(String),

    #[error(transparent)]
    Unexpected(#[from] anyhow::Error),
}

impl CliError {
    pub fn expected(msg: impl Into<String>) -> Self {
        Self::Expected(msg.into())
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        Self::Unexpected(e.into())
    }
}

impl From<capnp::Error> for CliError {
    fn from(e: capnp::Error) -> Self {
        Self::Unexpected(e.into())
    }
}
