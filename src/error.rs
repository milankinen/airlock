use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to create cache directory {path}: {source}")]
    CacheDir {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to download {url}: {source}")]
    Download {
        url: String,
        source: reqwest::Error,
    },

    #[error("checksum mismatch for {path}: expected {expected}, got {actual}")]
    Checksum {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error("VM configuration error: {0}")]
    VmConfig(String),

    #[error("VM runtime error: {0}")]
    VmRuntime(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
