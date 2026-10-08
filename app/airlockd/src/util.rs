//! Shared utilities for the in-VM supervisor.

use std::path::{Path, PathBuf};

/// Resolve a container path inside a container root, with chroot symlink
/// semantics.
///
/// Absolute symlink targets resolve relative to `root`, not to the host `/`.
/// Relative symlink targets resolve normally.
///
/// Example: `root = /mnt/overlay/rootfs`, `guest_path = /var/run/docker.sock`
/// and `var/run` is a symlink to `/run`. The result is
/// `/mnt/overlay/rootfs/run/docker.sock`, not `/run/docker.sock`.
/// Args:
///  - `root`: Container root directory on the guest
///  - `guest_path`: Path as the container sees it
///
/// Returns:
///   Resolved path on the guest. The path does not have to exist.
#[allow(dead_code)]
pub fn resolve_in_root(root: &Path, guest_path: &str) -> PathBuf {
    // Walk `guest_path` one component at a time and resolve the symlinks
    // at each component.
    let mut path = root.to_path_buf();
    for component in Path::new(guest_path).components() {
        match component {
            std::path::Component::Normal(name) => {
                path.push(name);
                // Resolve symlinks at this component, up to 40 hops.
                for _ in 0..40 {
                    match std::fs::read_link(&path) {
                        Ok(target) if target.is_absolute() => {
                            // Absolute target: relative to the container root.
                            let stripped = target.strip_prefix("/").unwrap_or(&target);
                            path = root.join(stripped);
                        }
                        Ok(target) => {
                            // Relative target: resolve from the symlink's directory.
                            path.pop();
                            path.push(target);
                        }
                        Err(_) => break, // not a symlink, or doesn't exist yet
                    }
                }
            }
            std::path::Component::RootDir => path = root.to_path_buf(),
            _ => {}
        }
    }
    path
}
