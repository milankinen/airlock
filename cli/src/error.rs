#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("VM configuration error: {0}")]
    VmConfig(String),

    #[error("VM runtime error: {0}")]
    VmRuntime(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
