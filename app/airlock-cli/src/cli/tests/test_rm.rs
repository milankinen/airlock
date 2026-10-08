use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::cache;
use crate::cli::cmd_rm::{RmArgs, run};
use crate::project::SandboxLock;
use crate::test_cfg::home::TempHome;

const FORCE: RmArgs = RmArgs { force: true };

/// A project under the temp home with a started sandbox whose image is
/// linked from the OCI cache, as `airlock start` leaves it.
struct Project {
    dir: PathBuf,
    image: PathBuf,
}

impl Project {
    fn started(home: &TempHome, name: &str) -> Self {
        let dir = home.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        drop(SandboxLock::acquire(&dir).unwrap());
        std::fs::write(dir.join(".airlock/sandbox/disk.img"), b"data").unwrap();
        let image = cache::image_path(&format!("sha256:{name}")).unwrap();
        std::fs::write(&image, br#"{"schema":"v2","image_layers":[]}"#).unwrap();
        std::fs::hard_link(&image, dir.join(".airlock/sandbox/image")).unwrap();
        Self { dir, image }
    }

    fn airlock(&self) -> PathBuf {
        self.dir.join(".airlock")
    }
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[test]
fn rm_of_project_removes_its_airlock_dir_and_unused_image() {
    let home = TempHome::new();
    let project = Project::started(&home, "proj");
    std::fs::write(project.airlock().join("airlock.toml"), "").unwrap();
    std::fs::write(project.airlock().join(".gitignore"), "*\n").unwrap();

    assert_eq!(run(&FORCE, &project.dir), 0);

    assert!(!exists(&project.airlock()));
    assert!(!project.image.exists());
    assert!(project.dir.is_dir());
}

#[test]
fn rm_of_airlock_dir_with_user_files_removes_only_sandbox() {
    let home = TempHome::new();
    let markers = [
        ("settings.yaml", false),
        ("config.json", false),
        ("vault.default.json", false),
        ("vault.default.enc.json", false),
        (crate::db::DIR, true),
        ("claude", true),
        ("codex", true),
        ("agents", true),
    ];

    for (marker, is_dir) in markers {
        let project = Project::started(&home, &format!("proj-{marker}"));
        let path = project.airlock().join(marker);
        if is_dir {
            std::fs::create_dir_all(&path).unwrap();
        } else {
            std::fs::write(&path, "{}").unwrap();
        }

        assert_eq!(run(&FORCE, &project.dir), 0, "{marker}");

        assert!(exists(&path), "{marker}");
        assert!(!exists(&project.airlock().join("sandbox")), "{marker}");
        assert!(!project.image.exists(), "{marker}");
    }
}

#[test]
fn rm_of_running_sandbox_is_refused() {
    let home = TempHome::new();
    let project = Project::started(&home, "proj");
    let _running = SandboxLock::acquire(&project.dir).unwrap();

    assert_eq!(run(&FORCE, &project.dir), 1);

    assert!(project.airlock().join("sandbox/disk.img").is_file());
    assert!(project.image.exists());
}

#[test]
fn rm_removes_symlinked_airlock_dir_or_sandbox_but_not_their_targets() {
    let home = TempHome::new();
    let victim = Project::started(&home, "victim");
    let victim_disk = victim.airlock().join("sandbox/disk.img");
    std::fs::write(victim.airlock().join("settings.yaml"), "").unwrap();

    let linked_airlock = home.path().join("linked-airlock");
    std::fs::create_dir_all(&linked_airlock).unwrap();
    symlink(victim.airlock(), linked_airlock.join(".airlock")).unwrap();
    assert_eq!(run(&FORCE, &linked_airlock), 0);
    assert!(!exists(&linked_airlock.join(".airlock")));

    for user_dir in [true, false] {
        let project = home.path().join(format!("linked-sandbox-{user_dir}"));
        std::fs::create_dir_all(project.join(".airlock")).unwrap();
        if user_dir {
            std::fs::write(project.join(".airlock/settings.yaml"), "").unwrap();
        }
        symlink(
            victim.airlock().join("sandbox"),
            project.join(".airlock/sandbox"),
        )
        .unwrap();
        assert_eq!(run(&FORCE, &project), 0);
        assert!(!exists(&project.join(".airlock/sandbox")));
        assert_eq!(exists(&project.join(".airlock")), user_dir);
    }

    assert!(victim.airlock().join("settings.yaml").is_file());
    assert!(victim_disk.is_file());
    assert!(victim.image.exists());
}
