use flate2::read::GzDecoder;
use std::path::Path;

/// Extract OCI image layers in order into a merged rootfs directory.
/// Handles whiteout files (.wh.*) for layer deletions.
pub fn extract_layers(layer_files: &[&Path], rootfs: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(rootfs)?;

    for layer_path in layer_files {
        let file = std::fs::File::open(layer_path)?;
        let gz = GzDecoder::new(file);
        let mut archive = tar::Archive::new(gz);

        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_path_buf();
            let path_str = path.to_string_lossy();

            // Handle whiteout files (OCI layer deletion markers)
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with(".wh.") {
                    let target_name = &name[4..]; // strip ".wh." prefix
                    if target_name == ".wh..opq" {
                        // Opaque whiteout: delete all siblings in this directory
                        if let Some(parent) = path.parent() {
                            let target_dir = rootfs.join(parent);
                            if target_dir.exists() {
                                for child in std::fs::read_dir(&target_dir)? {
                                    let child = child?;
                                    let _ = std::fs::remove_dir_all(child.path());
                                }
                            }
                        }
                    } else {
                        // Regular whiteout: delete the named file
                        if let Some(parent) = path.parent() {
                            let target = rootfs.join(parent).join(target_name);
                            let _ = std::fs::remove_file(&target);
                            let _ = std::fs::remove_dir_all(&target);
                        }
                    }
                    continue;
                }
            }

            // Skip paths that look problematic
            if path_str.contains("..") {
                continue;
            }

            let dest = rootfs.join(&path);
            entry.unpack(&dest).ok(); // ignore individual extraction errors
        }
    }

    Ok(())
}
