//! Test of the `claude` wrapper that the claude pack setup script writes.
//! The wrapper runs on the host with fake Claude Code versions.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use airlock_test_utils::temp_dir;

use crate::test_cfg::packs::sh;

/// Copy FIFO path in the wrapper. The test moves it into a temp directory.
const COPY_FIFO: &str = "/run/airlock/clipboard.copy";

/// Test that the wrapper runs the newest Claude Code version and sets
/// `WAYLAND_DISPLAY` only when clipboard copy exists. Without copy, Claude
/// Code must not try wl-copy.
///   1. Take the wrapper from the setup script
///   2. Add fake versions 2.1.9, 2.1.10 and the partial file 2.1.11.tmp
///   3. Run without the copy FIFO and check that 2.1.10 runs with the args
///      and without `WAYLAND_DISPLAY`
///   4. Make the copy FIFO and check that `WAYLAND_DISPLAY` is `airlock-0`
#[test]
fn wrapper_runs_newest_version_and_sets_wayland_display_only_with_copy() {
    let packs = crate::packs::init().unwrap();
    let setup = packs
        .packs
        .iter()
        .find(|p| p.metadata().name == "claude")
        .and_then(|p| p.0.setup)
        .unwrap();
    let start = setup.find("<<'WRAPPER'\n").unwrap() + "<<'WRAPPER'\n".len();
    let end = start + setup[start..].find("\nWRAPPER\n").unwrap();

    let dir = temp_dir();
    let fifo = dir.path().join("copy");
    let wrapper = dir.path().join("claude");
    std::fs::write(
        &wrapper,
        setup[start..end].replace(COPY_FIFO, &fifo.display().to_string()),
    )
    .unwrap();
    let versions = dir.path().join(".local/share/claude/versions");
    std::fs::create_dir_all(&versions).unwrap();
    for version in ["2.1.9", "2.1.10", "2.1.11.tmp"] {
        let path = versions.join(version);
        let fake = format!("#!/bin/sh\necho \"{version} ${{WAYLAND_DISPLAY-unset}} $*\"\n");
        std::fs::write(&path, fake).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let run = || {
        let out = Command::new(sh())
            .arg(&wrapper)
            .args(["-p", "a b"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", dir.path())
            .env("WAYLAND_DISPLAY", "airlock-0")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };

    assert_eq!(run(), "2.1.10 unset -p a b\n");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(run(), "2.1.10 airlock-0 -p a b\n");
}
