//! Tests for the resolution of the `[mounts]` config into host sources,
//! guest targets and mount types, and for missing sources.

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
    /// A new host in a temporary directory, with empty home and project
    /// directories.
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
        self.resolve_shared(toml, true)
    }

    /// As [`Self::resolve`]. `project_share` tells if the sandbox can
    /// write the project directory.
    fn resolve_shared(
        &self,
        toml: &str,
        project_share: bool,
    ) -> anyhow::Result<Vec<ResolvedMount>> {
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
            project_share,
        )
    }
}

/// The permission bits of `path`.
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Each mount as (key, source, target, read-only, is a directory).
fn summary(mounts: &[ResolvedMount]) -> Vec<(String, PathBuf, String, bool, bool)> {
    mounts
        .iter()
        .map(|m| {
            (
                m.key().to_string(),
                m.source.to_path().unwrap().to_path_buf(),
                m.target.clone(),
                m.read_only,
                matches!(m.mount_type, MountType::Dir { .. }),
            )
        })
        .collect()
}

/// Test that config mounts resolve their sources on the host and their
/// targets in the guest, so that each path form maps to the correct place.
///   1. Configure mounts with absolute, relative, `./`, `~` and file sources
///      and with absolute, relative and `~` targets, plus a disabled mount
///   2. Resolve the mounts
///   3. Check the source, target, read-only flag and type of each mount
///   4. Check that directory mounts get numbered tags, the file mount keeps
///      its config key, and the disabled mount is absent
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

    // Mounts resolve in key order. Directory mounts get the VirtioFS tags
    // `dir_0`, `dir_1`, ... and file mounts keep their config key.
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

/// Test that a missing mount source is an error by default, and that the
/// `ignore` and `warn` options skip the mount and create nothing.
///   1. Resolve a mount with a missing absolute source, then with a missing
///      `~` source, and check that each gives an error
///   2. Resolve two missing mounts with `missing = "ignore"` and `"warn"`
///   3. Check that no mounts resolve and no source directory exists
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

/// Test that the `create-dir` and `create-file` options create a missing
/// source with the correct type, content and mode.
///   1. Resolve mounts that create nested directories and files, some with
///      a custom mode and file content
///   2. Check the mount types, the file contents and the modes (default
///      0755 for directories and 0644 for files)
///   3. Resolve a mount with an invalid octal mode and check the error
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

/// Test that a mount source through a symlink in the project, which the
/// sandbox can write, is refused when the link points out of the project.
/// The sandbox can change such a link and get any host directory.
///   1. Make a directory outside the project and a link to it in the project
///   2. Resolve a mount of the link and a `create-dir` mount below it
///   3. Check that both are refused with the real path, and that nothing
///      was created outside
///   4. Check that the real path and a link inside the project are allowed
#[test]
fn mount_source_through_project_link_to_outside_is_refused() {
    let host = Host::new();
    let outside = host.home.join("secret");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(host.project.join("real")).unwrap();
    std::os::unix::fs::symlink(&outside, host.project.join("out")).unwrap();
    std::os::unix::fs::symlink("real", host.project.join("in")).unwrap();

    for toml in [
        "[mounts.x]\nsource = \"out\"\ntarget = \"/x\"\n",
        "[mounts.x]\nsource = \"./out/new\"\ntarget = \"/x\"\nmissing = \"create-dir\"\n",
    ] {
        let err = host.resolve(toml).unwrap_err().to_string();
        assert!(err.contains("Set the real path"), "{err}");
        assert!(err.contains(&outside.display().to_string()), "{err}");
    }
    assert!(!outside.join("new").exists());

    let mounts = host
        .resolve(&format!(
            "[mounts.a]\nsource = \"{}\"\ntarget = \"/a\"\n\
             [mounts.b]\nsource = \"in\"\ntarget = \"/b\"\n",
            outside.display()
        ))
        .unwrap();
    let sources: Vec<_> = summary(&mounts).into_iter().map(|m| m.1).collect();
    assert_eq!(sources, [outside, host.project.join("real")]);
}

/// Test that `create-file` does not follow a dangling symlink at the mount
/// source. The sandbox can put such a link in the project to make the host
/// write a file at a place that the sandbox picks.
///   1. Put a dangling link in the project that points to a missing file
///      outside the project
///   2. Resolve a `create-file` mount of the link
///   3. Check that it fails and the file outside does not exist
#[test]
fn create_file_mount_source_does_not_follow_dangling_link() {
    let host = Host::new();
    let victim = host.home.join(".bashrc");
    std::os::unix::fs::symlink(&victim, host.project.join("planted")).unwrap();

    host.resolve("[mounts.x]\nsource = \"planted\"\ntarget = \"/x\"\nmissing = \"create-file\"\n")
        .unwrap_err();
    assert!(!victim.exists());
}

/// Test that a symlink in a writable dir mount in the project must stay in
/// that mount, also when it points to an other place in the project. The
/// project counts only when the sandbox shares it.
///   1. Make a writable dir mount `work` in the project, and in it a link to
///      an other project dir
///   2. Check that a mount through the link is refused
///   3. Check that a project link to outside is allowed when the project is
///      not shared
#[test]
fn link_in_nested_writable_mount_must_stay_in_mount() {
    let host = Host::new();
    std::fs::create_dir_all(host.project.join("work")).unwrap();
    std::fs::create_dir_all(host.project.join("other")).unwrap();
    std::os::unix::fs::symlink(host.project.join("other"), host.project.join("work/link")).unwrap();

    let err = host
        .resolve(
            "[mounts.a]\nsource = \"work\"\ntarget = \"/a\"\n\
             [mounts.b]\nsource = \"work/link\"\ntarget = \"/b\"\nread_only = true\n",
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("Set the real path"), "{err}");

    std::os::unix::fs::symlink(&host.home, host.project.join("out")).unwrap();
    host.resolve_shared("[mounts.x]\nsource = \"out\"\ntarget = \"/x\"\n", false)
        .unwrap();
}

/// Test that a link chain that goes through the project is checked at each
/// link. A user link outside the project can point to a project path that
/// is a link of the sandbox.
///   1. Make a user link `~/dot` to `<project>/x`, and a link `x` to a dir
///      outside the project
///   2. Check that a mount of `~/dot` is refused
#[test]
fn link_chain_through_project_is_refused() {
    let host = Host::new();
    let outside = host.home.join("secret");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, host.project.join("x")).unwrap();
    std::os::unix::fs::symlink(host.project.join("x"), host.home.join("dot")).unwrap();

    let err = host
        .resolve("[mounts.x]\nsource = \"~/dot\"\ntarget = \"/x\"\n")
        .unwrap_err()
        .to_string();
    assert!(err.contains("Set the real path"), "{err}");
}
