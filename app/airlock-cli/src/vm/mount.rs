//! Mount resolution: expand paths, classify dir vs file mounts.

use std::path::{Path, PathBuf};

/// A mount with host/guest paths fully expanded and validated.
#[derive(Debug)]
pub struct ResolvedMount {
    /// Mount type: file / directory
    pub mount_type: MountType,
    /// Expanded absolute source path on host.
    pub source: PathBuf,
    /// Expanded absolute target path in container.
    pub target: String,
    pub read_only: bool,
}

/// Whether a mount is a directory (VirtioFS share) or a single file.
#[derive(Debug)]
pub enum MountType {
    Dir {
        key: String,
    },
    /// File mounts are hard-linked (with copy fallback) into the project
    /// overlay directory under `files/{rw|ro}/{mount_key}`, and exposed via
    /// `files/rw` / `files/ro` VirtioFS shares. Inside the container, the
    /// target path becomes a symlink → `/airlock/.files/{rw|ro}/{mount_key}`.
    File {
        mount_key: String,
    },
}

impl ResolvedMount {
    /// VirtioFS share tag (for Dir mounts) or config key (for File mounts).
    pub fn key(&self) -> &str {
        match &self.mount_type {
            MountType::Dir { key } => key.as_str(),
            MountType::File { mount_key } => mount_key.as_str(),
        }
    }

    /// Debug path: where this mount is accessible in the VM environment.
    pub fn vm_path(&self) -> String {
        match &self.mount_type {
            MountType::Dir { key } => format!("/mnt/{key}"),
            MountType::File { mount_key } => {
                let rw_or_ro = if self.read_only { "ro" } else { "rw" };
                format!("/airlock/.files/{rw_or_ro}/{mount_key}")
            }
        }
    }
}

/// Expand `~` in mount paths, handle missing sources, and classify as
/// dir or file mounts.
pub fn resolve_mounts(
    mounts: &[(&str, crate::config::config::Mount)],
    host_home: &Path,
    container_home: &str,
    cwd: &Path,
    guest_cwd: &Path,
) -> anyhow::Result<Vec<ResolvedMount>> {
    use std::os::unix::fs::PermissionsExt;

    use crate::config::config::MissingAction;

    let container_home = PathBuf::from(container_home);
    let mut result = Vec::new();

    let mut dir_idx: usize = 0;
    for (name, m) in mounts {
        let source = crate::util::expand_tilde(&m.source, host_home);
        // Resolve relative paths against cwd
        let source = if source.is_relative() {
            cwd.join(&source)
        } else {
            source
        };

        // Handle missing source
        if !source.exists() {
            match m.missing {
                MissingAction::Fail => {
                    anyhow::bail!("mount source does not exist: {}", source.display());
                }
                MissingAction::Warn => {
                    crate::cli::log!(
                        "  {} mount skipped (not found): {}",
                        crate::cli::bullet(),
                        crate::cli::dim(&source.display().to_string())
                    );
                    continue;
                }
                MissingAction::Ignore => continue,
                MissingAction::CreateDir => {
                    std::fs::create_dir_all(&source)?;
                    let mode = parse_mode(m.create_mode.as_deref(), 0o755)?;
                    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(mode))?;
                }
                MissingAction::CreateFile => {
                    if let Some(parent) = source.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let content = m.file_content.as_deref().unwrap_or("");
                    std::fs::write(&source, content)?;
                    let mode = parse_mode(m.create_mode.as_deref(), 0o644)?;
                    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(mode))?;
                }
            }
        }

        let source = std::fs::canonicalize(&source).unwrap_or(source);
        let target = crate::util::expand_tilde(&m.target, &container_home);
        // Resolve relative target paths against guest_cwd (mirrors source → cwd behavior)
        let target = if target.is_relative() {
            guest_cwd.join(&target)
        } else {
            target
        };

        // Dir mounts get indexed tags (dir_0, dir_1, …) sorted by config key.
        // File mounts use the config key as their identifier.
        let mount_type = if source.is_dir() {
            let key = format!("dir_{dir_idx}");
            dir_idx += 1;
            MountType::Dir { key }
        } else {
            MountType::File {
                mount_key: name.to_string(),
            }
        };

        result.push(ResolvedMount {
            source,
            mount_type,
            target: target.to_string_lossy().to_string(),
            read_only: m.read_only,
        });
    }

    Ok(result)
}

/// Parse an octal mode string (e.g. "755") into a `u32`, or return the default.
fn parse_mode(s: Option<&str>, default: u32) -> anyhow::Result<u32> {
    match s {
        Some(s) => {
            u32::from_str_radix(s, 8).map_err(|_| anyhow::anyhow!("invalid octal mode: {s:?}"))
        }
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{MountType, resolve_mounts};
    use crate::config::config::{MissingAction, Mount};

    fn mount(source: &str, target: &str) -> Mount {
        Mount {
            enabled: true,
            source: source.into(),
            target: target.into(),
            read_only: false,
            missing: MissingAction::Fail,
            create_mode: None,
            file_content: None,
        }
    }

    fn mount_with(source: &str, target: &str, missing: MissingAction) -> Mount {
        Mount {
            enabled: true,
            source: source.into(),
            target: target.into(),
            read_only: false,
            missing,
            create_mode: None,
            file_content: None,
        }
    }

    #[test]
    fn absolute_existing_dir() {
        let tmp = tempdir();
        let src = tmp.join("mydir");
        std::fs::create_dir(&src).unwrap();

        let mounts = resolve_mounts(
            &[("mydir", mount(&src.to_string_lossy(), "/container/data"))],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].target, "/container/data");
        assert!(matches!(mounts[0].mount_type, MountType::Dir { .. }));
    }

    #[test]
    fn relative_path_resolved_against_cwd() {
        let tmp = tempdir();
        let src = tmp.join("rel-dir");
        std::fs::create_dir(&src).unwrap();

        let mounts = resolve_mounts(
            &[("rel-dir", mount("rel-dir", "/target"))],
            Path::new("/home/test"),
            "/root",
            &tmp, // cwd
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        // Source should be the absolute resolved path
        assert_eq!(mounts[0].source, std::fs::canonicalize(&src).unwrap());
    }

    #[test]
    fn dot_slash_relative_path() {
        let tmp = tempdir();
        let src = tmp.join("dot-dir");
        std::fs::create_dir(&src).unwrap();

        let mounts = resolve_mounts(
            &[("dot-dir", mount("./dot-dir", "/target"))],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source, std::fs::canonicalize(&src).unwrap());
    }

    #[test]
    fn missing_fail_is_default() {
        let tmp = tempdir();

        let result = resolve_mounts(
            &[("missing", mount("/nonexistent/path", "/target"))],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        );

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("does not exist"),
            "expected 'does not exist' error, got: {err}"
        );
    }

    #[test]
    fn missing_ignore_skips_silently() {
        let tmp = tempdir();

        let mounts = resolve_mounts(
            &[(
                "missing",
                mount_with("/nonexistent", "/target", MissingAction::Ignore),
            )],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert!(mounts.is_empty(), "ignored mount should be skipped");
    }

    #[test]
    fn missing_warn_skips() {
        let tmp = tempdir();

        let mounts = resolve_mounts(
            &[(
                "missing",
                mount_with("/nonexistent", "/target", MissingAction::Warn),
            )],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert!(mounts.is_empty(), "warned mount should be skipped");
    }

    #[test]
    fn missing_create_makes_directory() {
        let tmp = tempdir();
        let src = tmp.join("auto-created");
        assert!(!src.exists());

        let mounts = resolve_mounts(
            &[(
                "auto",
                mount_with(&src.to_string_lossy(), "/target", MissingAction::CreateDir),
            )],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert!(src.exists(), "directory should have been created");
        assert!(src.is_dir());
    }

    #[test]
    fn missing_create_nested() {
        let tmp = tempdir();
        let src = tmp.join("a/b/c/deep");
        assert!(!src.exists());

        let mounts = resolve_mounts(
            &[(
                "deep",
                mount_with(&src.to_string_lossy(), "/target", MissingAction::CreateDir),
            )],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert!(src.exists(), "nested directory should have been created");
    }

    #[test]
    fn missing_create_relative() {
        let tmp = tempdir();

        let mounts = resolve_mounts(
            &[(
                "rel",
                mount_with("new-relative-dir", "/target", MissingAction::CreateDir),
            )],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        let expected = tmp.join("new-relative-dir");
        assert!(
            expected.exists(),
            "relative dir should be created under cwd"
        );
    }

    #[test]
    fn tilde_expansion_source() {
        let tmp = tempdir();
        let home = tmp.join("fakehome");
        let src = home.join(".config");
        std::fs::create_dir_all(&src).unwrap();

        let mounts = resolve_mounts(
            &[("config", mount("~/.config", "/config"))],
            &home,
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source, std::fs::canonicalize(&src).unwrap());
    }

    #[test]
    fn tilde_expansion_target() {
        let tmp = tempdir();
        let src = tmp.join("data");
        std::fs::create_dir(&src).unwrap();

        let mounts = resolve_mounts(
            &[("data", mount(&src.to_string_lossy(), "~/data"))],
            Path::new("/home/test"),
            "/home/container",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].target, "/home/container/data");
    }

    #[test]
    fn relative_target_resolved_against_guest_cwd() {
        let tmp = tempdir();
        let src = tmp.join("myfile.txt");
        std::fs::write(&src, "content").unwrap();

        let mounts = resolve_mounts(
            &[("myfile", mount(&src.to_string_lossy(), "myfile.txt"))],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].target, "/workdir/myfile.txt");
    }

    #[test]
    fn file_mount() {
        let tmp = tempdir();
        let src = tmp.join("myfile.txt");
        std::fs::write(&src, "content").unwrap();

        let mounts = resolve_mounts(
            &[(
                "myfile",
                mount(&src.to_string_lossy(), "/container/file.txt"),
            )],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert!(matches!(mounts[0].mount_type, MountType::File { .. }));
        // Mount key matches the config key name
        if let MountType::File { mount_key } = &mounts[0].mount_type {
            assert_eq!(mount_key, "myfile");
        }
    }

    #[test]
    fn multiple_mounts_mixed() {
        let tmp = tempdir();
        let dir1 = tmp.join("dir1");
        std::fs::create_dir(&dir1).unwrap();
        let dir2 = tmp.join("dir2");
        std::fs::create_dir(&dir2).unwrap();

        let mounts = resolve_mounts(
            &[
                ("a", mount(&dir1.to_string_lossy(), "/a")),
                ("b", mount_with("/nonexistent", "/b", MissingAction::Ignore)),
                ("c", mount(&dir2.to_string_lossy(), "/c")),
            ],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 2, "ignored mount should be skipped");
        assert_eq!(mounts[0].target, "/a");
        assert_eq!(mounts[1].target, "/c");
    }

    #[test]
    fn read_only_preserved() {
        let tmp = tempdir();
        let src = tmp.join("ro-dir");
        std::fs::create_dir(&src).unwrap();

        let mounts = resolve_mounts(
            &[(
                "ro",
                Mount {
                    enabled: true,
                    source: src.to_string_lossy().into(),
                    target: "/ro".into(),
                    read_only: true,
                    missing: MissingAction::Fail,
                    create_mode: None,
                    file_content: None,
                },
            )],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert!(mounts[0].read_only);
    }

    #[test]
    fn tilde_missing_fail() {
        let tmp = tempdir();
        let home = tmp.join("fakehome");
        std::fs::create_dir(&home).unwrap();
        // ~/.nonexistent doesn't exist → fail
        let result = resolve_mounts(
            &[("x", mount("~/.nonexistent", "/target"))],
            &home,
            "/root",
            &tmp,
            Path::new("/workdir"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn tilde_missing_create() {
        let tmp = tempdir();
        let home = tmp.join("fakehome");
        std::fs::create_dir(&home).unwrap();

        let mounts = resolve_mounts(
            &[(
                "auto",
                mount_with("~/.auto-created", "/target", MissingAction::CreateDir),
            )],
            &home,
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        let expected = home.join(".auto-created");
        assert!(expected.exists(), "should create under home dir");
        assert!(expected.is_dir());
    }

    #[test]
    fn tilde_missing_ignore() {
        let tmp = tempdir();
        let home = tmp.join("fakehome");
        std::fs::create_dir(&home).unwrap();

        let mounts = resolve_mounts(
            &[(
                "nope",
                mount_with("~/.nope", "/target", MissingAction::Ignore),
            )],
            &home,
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert!(mounts.is_empty());
        assert!(!home.join(".nope").exists(), "should not create anything");
    }

    #[test]
    fn bare_tilde_source() {
        let tmp = tempdir();
        let home = tmp.join("fakehome");
        std::fs::create_dir(&home).unwrap();

        let mounts = resolve_mounts(
            &[("home", mount("~", "~"))],
            &home,
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source, std::fs::canonicalize(&home).unwrap());
        assert_eq!(mounts[0].target, "/root");
    }

    #[test]
    fn missing_create_file() {
        let tmp = tempdir();
        let src = tmp.join("new-file.txt");
        assert!(!src.exists());

        let mounts = resolve_mounts(
            &[(
                "f",
                mount_with(&src.to_string_lossy(), "/target", MissingAction::CreateFile),
            )],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert!(src.exists(), "file should have been created");
        assert!(src.is_file());
        assert_eq!(std::fs::read_to_string(&src).unwrap(), "");
        assert!(matches!(mounts[0].mount_type, MountType::File { .. }));
    }

    #[test]
    fn missing_create_file_with_content() {
        let tmp = tempdir();
        let src = tmp.join("init.json");
        assert!(!src.exists());

        let mounts = resolve_mounts(
            &[(
                "f",
                Mount {
                    enabled: true,
                    source: src.to_string_lossy().into(),
                    target: "/target".into(),
                    read_only: false,
                    missing: MissingAction::CreateFile,
                    create_mode: None,
                    file_content: Some("{}".into()),
                },
            )],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(std::fs::read_to_string(&src).unwrap(), "{}");
    }

    #[test]
    fn missing_create_file_nested_parent() {
        let tmp = tempdir();
        let src = tmp.join("a/b/c/deep.txt");
        assert!(!src.exists());

        let mounts = resolve_mounts(
            &[(
                "f",
                mount_with(&src.to_string_lossy(), "/target", MissingAction::CreateFile),
            )],
            Path::new("/home/test"),
            "/root",
            &tmp,
            Path::new("/workdir"),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert!(src.exists(), "file with nested parents should be created");
        assert!(src.is_file());
    }

    /// Create a temporary directory that's cleaned up on drop.
    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("airlock-test-{}", std::process::id()));
        let dir = dir.join(format!(
            "{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
