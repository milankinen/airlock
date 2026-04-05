#[cfg(target_os = "macos")]
use std::ffi::CString;
use std::path::{Path, PathBuf};

pub fn cache_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
    let dir = PathBuf::from(home).join(".ezpez");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn image_dir(digest: &str) -> anyhow::Result<PathBuf> {
    let name = digest.split(':').next_back().unwrap_or(digest);
    let dir = cache_dir()?.join("images").join(name);
    Ok(dir)
}

pub fn project_dir(project_hash: &str) -> anyhow::Result<PathBuf> {
    let dir = cache_dir()?.join("projects").join(project_hash);
    Ok(dir)
}

/// Recursive directory copy using APFS clonefile (CoW).
/// Falls back to regular copy if clonefile is not supported.
pub fn cow_copy(src: &Path, dst: &Path) -> anyhow::Result<()> {
    if dst.exists() {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let src_c = CString::new(src.to_string_lossy().as_bytes())?;
        let dst_c = CString::new(dst.to_string_lossy().as_bytes())?;
        if unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) } == 0 {
            return Ok(());
        }
    }

    copy_dir_recursive(src, dst)?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(&src_path)?;
            std::os::unix::fs::symlink(&target, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
