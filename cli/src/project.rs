use sha2::{Digest, Sha256};

/// Generate a stable project hash from the current working directory.
/// This hash identifies the project's persistent sandbox state.
pub fn project_hash() -> anyhow::Result<String> {
    let cwd = std::env::current_dir()?;
    let canonical = std::fs::canonicalize(&cwd).unwrap_or(cwd);

    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    Ok(hex::encode(&hasher.finalize()[..16]))
}
