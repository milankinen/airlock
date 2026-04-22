//! Shared utilities for the in-VM supervisor.

use std::path::{Path, PathBuf};

/// Resolve `guest_path` within `root` using chroot-aware symlink semantics.
///
/// Standard path resolution follows absolute symlinks from the process root
/// (`/`). Inside a chroot/container, absolute symlinks are meant to be
/// interpreted relative to the container root, not the host root.
///
/// This function walks `guest_path` component by component: when it encounters
/// a symlink whose target is absolute, the target is joined to `root` rather
/// than to `/`. Relative symlink targets are resolved normally.
///
/// Example: with `root = /mnt/overlay/rootfs` and `guest_path = /var/run/docker.sock`,
/// if `var/run` is a symlink to `/run`, this returns
/// `/mnt/overlay/rootfs/run/docker.sock` rather than `/run/docker.sock`.
#[allow(dead_code)]
pub fn resolve_in_root(root: &Path, guest_path: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for component in Path::new(guest_path).components() {
        match component {
            std::path::Component::Normal(name) => {
                path.push(name);
                // Resolve symlinks at this component, up to 40 hops.
                for _ in 0..40 {
                    match std::fs::read_link(&path) {
                        Ok(target) if target.is_absolute() => {
                            // Absolute target: treat as relative to container root.
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
