use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::test_cfg::{TempDir, resolve_project_toml, temp_dir};
use crate::vm::mount::{MountType, ResolvedMount, resolve_mounts};

/// A host with a home directory and a project directory (the cwd).
struct Host {
    _dir: TempDir,
    home: PathBuf,
    project: PathBuf,
}

impl Host {
    fn new() -> Self {
        let dir = temp_dir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        let project = root.join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        Self {
            _dir: dir,
            home,
            project,
        }
    }

    /// The enabled `[mounts]` of the project config `toml`, resolved for
    /// a container whose home is `/home/user` and cwd `/workdir`.
    fn resolve(&self, toml: &str) -> anyhow::Result<Vec<ResolvedMount>> {
        let config = resolve_project_toml(toml)?;
        let mounts: Vec<_> = config
            .values
            .mounts
            .iter()
            .filter(|(_, m)| m.enabled)
            .map(|(key, m)| (key.as_str(), m.clone()))
            .collect();
        resolve_mounts(
            &mounts,
            &self.home,
            "/home/user",
            &self.project,
            Path::new("/workdir"),
        )
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn summary(mounts: &[ResolvedMount]) -> Vec<(String, PathBuf, String, bool, bool)> {
    mounts
        .iter()
        .map(|m| {
            (
                m.key().to_string(),
                m.source.clone(),
                m.target.clone(),
                m.read_only,
                matches!(m.mount_type, MountType::Dir { .. }),
            )
        })
        .collect()
}

#[test]
fn config_mounts_resolve_host_sources_and_guest_targets() {
    let host = Host::new();
    let data = host.project.parent().unwrap().join("data");
    for dir in [
        data.clone(),
        host.project.join("rel-dir"),
        host.project.join("dot-dir"),
        host.home.join(".config"),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(host.project.join("notes.txt"), "n").unwrap();

    let mounts = host
        .resolve(&format!(
            r#"
            [mounts.a-abs]
            source = "{}"
            target = "/data"
            read_only = true

            [mounts.b-rel]
            source = "rel-dir"
            target = "rel"

            [mounts.c-dot]
            source = "./dot-dir"
            target = "~/dot"

            [mounts.d-tilde]
            source = "~/.config"
            target = "/config"

            [mounts.e-file]
            source = "notes.txt"
            target = "notes.txt"

            [mounts.f-home]
            source = "~"
            target = "~"

            [mounts.g-disabled]
            enabled = false
            source = "/nonexistent"
            target = "/x"
            "#,
            data.display()
        ))
        .unwrap();

    let s = |p: &str| p.to_string();
    assert_eq!(
        summary(&mounts),
        [
            (s("dir_0"), data, s("/data"), true, true),
            (
                s("dir_1"),
                host.project.join("rel-dir"),
                s("/workdir/rel"),
                false,
                true
            ),
            (
                s("dir_2"),
                host.project.join("dot-dir"),
                s("/home/user/dot"),
                false,
                true
            ),
            (
                s("dir_3"),
                host.home.join(".config"),
                s("/config"),
                false,
                true
            ),
            (
                s("e-file"),
                host.project.join("notes.txt"),
                s("/workdir/notes.txt"),
                false,
                false
            ),
            (s("dir_4"), host.home.clone(), s("/home/user"), false, true),
        ]
    );
}

#[test]
fn missing_mount_source_fails_unless_skipped() {
    let host = Host::new();

    for source in ["/nonexistent/path", "~/.nonexistent"] {
        let err = host
            .resolve(&format!(
                "[mounts.x]\nsource = \"{source}\"\ntarget = \"/target\"\n"
            ))
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    let mounts = host
        .resolve(
            r#"
            [mounts.ignored]
            source = "~/.nope"
            target = "/a"
            missing = "ignore"

            [mounts.warned]
            source = "/nonexistent"
            target = "/b"
            missing = "warn"
            "#,
        )
        .unwrap();
    assert!(mounts.is_empty());
    assert!(!host.home.join(".nope").exists());
}

#[test]
fn missing_mount_source_is_created_on_request() {
    let host = Host::new();

    let mounts = host
        .resolve(
            r#"
            [mounts.a-nested-dir]
            source = "a/b/c/deep"
            target = "/deep"
            missing = "create-dir"

            [mounts.b-home-dir]
            source = "~/.auto-created"
            target = "/auto"
            missing = "create-dir"
            create_mode = "700"

            [mounts.c-empty-file]
            source = "new-file.txt"
            target = "/new"
            missing = "create-file"

            [mounts.d-seeded-file]
            source = "x/y/init.json"
            target = "/init.json"
            missing = "create-file"
            file_content = "{}"
            create_mode = "600"
            "#,
        )
        .unwrap();

    let types: Vec<bool> = mounts
        .iter()
        .map(|m| matches!(m.mount_type, MountType::Dir { .. }))
        .collect();
    assert_eq!(types, [true, true, false, false]);
    assert_eq!(mode(&host.project.join("a/b/c/deep")), 0o755);
    assert_eq!(mode(&host.home.join(".auto-created")), 0o700);
    let empty = host.project.join("new-file.txt");
    assert_eq!(std::fs::read_to_string(&empty).unwrap(), "");
    assert_eq!(mode(&empty), 0o644);
    let seeded = host.project.join("x/y/init.json");
    assert_eq!(std::fs::read_to_string(&seeded).unwrap(), "{}");
    assert_eq!(mode(&seeded), 0o600);

    let err = host
        .resolve(
            "[mounts.x]\nsource = \"bad\"\ntarget = \"/x\"\nmissing = \"create-dir\"\ncreate_mode = \"9z\"\n",
        )
        .unwrap_err();
    assert!(err.to_string().contains("invalid octal mode"), "{err}");
}
