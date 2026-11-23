//! Shared utilities for the in-VM supervisor.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Resolve a container path inside a container root, with chroot symlink
/// semantics.
///
/// Absolute symlink targets resolve relative to `root`, not to the host `/`.
/// Relative symlink targets resolve from the directory of the link. A `..`
/// at `root` stays at `root`, so the result is always below `root`.
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
    // Names below `root` that are resolved. No name is a symlink, except
    // the last name while it is checked.
    let mut resolved: Vec<OsString> = Vec::new();
    // Names to walk, in reverse order. A symlink puts its target here.
    let mut todo: Vec<OsString> = Vec::new();
    push_components(&mut todo, Path::new(guest_path));
    let mut hops = 0;
    while let Some(name) = todo.pop() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        resolved.push(name);
        // Stop at 40 hops, as the kernel does. The rest of the path then
        // stays as it is.
        if hops >= 40 {
            continue;
        }
        let path: PathBuf = std::iter::once(root.as_os_str())
            .chain(resolved.iter().map(OsString::as_os_str))
            .collect();
        // An error means: not a symlink, or the path does not exist.
        if let Ok(target) = std::fs::read_link(&path) {
            hops += 1;
            resolved.pop();
            if target.is_absolute() {
                resolved.clear();
            }
            push_components(&mut todo, &target);
        }
    }
    std::iter::once(root.as_os_str())
        .chain(resolved.iter().map(OsString::as_os_str))
        .collect()
}

/// Put the names of `path` on the `todo` stack, so that the first name is
/// on top. `..` stays as a name. The root and `.` are dropped.
pub(crate) fn push_components(todo: &mut Vec<OsString>, path: &Path) {
    for component in path.components().rev() {
        match component {
            Component::Normal(name) => todo.push(name.to_owned()),
            Component::ParentDir => todo.push("..".into()),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;

    use airlock_test_utils::temp_dir;

    use super::resolve_in_root;

    /// Test that a relative symlink cannot go above the container root. A
    /// link with many `..` must not point to a guest path outside of the
    /// rootfs.
    ///   1. Make a link `a/x -> ../../../../etc` in the root
    ///   2. Make a link `a/y -> ../b`
    ///   3. Check that `a/x/passwd` resolves to `<root>/etc/passwd`
    ///   4. Check that `a/y/f` resolves to `<root>/b/f`
    #[test]
    fn relative_link_above_root_is_clamped_to_root() {
        let tmp = temp_dir();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("a")).unwrap();
        symlink("../../../../etc", root.join("a/x")).unwrap();
        symlink("../b", root.join("a/y")).unwrap();

        assert_eq!(
            resolve_in_root(&root, "/a/x/passwd"),
            root.join("etc/passwd")
        );
        assert_eq!(resolve_in_root(&root, "/a/y/f"), root.join("b/f"));
        assert_eq!(resolve_in_root(&root, "/../../a"), root.join("a"));
    }

    /// Test that an absolute symlink, also in the middle of a link chain,
    /// resolves inside the root and not on the guest.
    ///   1. Make `var/run -> /run` and `run/l -> /usr/lib`
    ///   2. Check that `/var/run/l/x` resolves to `<root>/usr/lib/x`
    #[test]
    fn absolute_link_resolves_inside_root() {
        let tmp = temp_dir();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("var")).unwrap();
        std::fs::create_dir_all(root.join("run")).unwrap();
        symlink("/run", root.join("var/run")).unwrap();
        symlink("/usr/lib", root.join("run/l")).unwrap();

        assert_eq!(
            resolve_in_root(&root, "/var/run/l/x"),
            root.join("usr/lib/x")
        );
    }
}
