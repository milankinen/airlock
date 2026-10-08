//! Tests of the setup scripts and the shared `lib.sh` helpers. The
//! scripts run in a host shell with stub commands.

use std::process::Command;

use crate::packs::install::compose::script;
use crate::test_cfg::packs::{HostShell, ShellRun, sh};

/// The name and setup script of each built-in pack that has one.
fn builtin_setups() -> Vec<(String, &'static str)> {
    crate::packs::init()
        .unwrap()
        .packs
        .iter()
        .filter_map(|p| Some((p.metadata().name.clone(), p.0.setup?)))
        .collect()
}

/// Run `body` as a full install script in a new host shell.
fn run(os_release: Option<&str>, body: &str, env: &[(&str, &str)]) -> ShellRun {
    HostShell::new(os_release).run(&script(body), env)
}

/// The exit code and the detected distro family of a host shell with
/// `os_release` (`None`: no os-release file).
fn distro(os_release: Option<&str>) -> (Option<i32>, String) {
    // The wrapper sends stdout to stderr, so the echo is the last line of
    // stderr.
    let run = run(os_release, "echo \"$DISTRO\"", &[]);
    let last = run.stderr.lines().last().unwrap_or_default().to_string();
    (run.code, last)
}

/// Test that each built-in setup script is valid POSIX sh, and that only
/// the mise pack uses mise.
///   1. Check that the shared helpers do not mention mise
///   2. Check each built-in setup script with `sh -n`
///   3. Check that no setup script other than the mise pack mentions mise
#[test]
fn builtin_setup_scripts_pass_sh_syntax_check_and_only_mise_pack_uses_mise() {
    assert!(!script("").contains("mise"));
    let setups = builtin_setups();
    assert!(!setups.is_empty());
    for (name, setup) in setups {
        let out = Command::new(sh())
            .args(["-n", "-c", &script(setup)])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        if name != "mise" {
            assert!(!setup.contains("mise"), "{name} mentions mise");
        }
    }
}

/// Test that the distro detection reads `ID` and `ID_LIKE` from
/// os-release, and fails with exit code 10 for other distros.
///   1. Check that Alpine, Debian and Debian-like os-release files give
///      the correct family
///   2. Check that an unknown distro, a missing file and a file that
///      does not parse give exit code 10
#[test]
fn distro_detection_follows_os_release_id_and_id_like() {
    assert_eq!(distro(Some("ID=alpine\n")), (Some(0), "alpine".into()));
    assert_eq!(distro(Some("ID=\"debian\"\n")), (Some(0), "debian".into()));
    assert_eq!(
        distro(Some("ID=ubuntu\nID_LIKE=debian\n")),
        (Some(0), "debian".into())
    );
    assert_eq!(
        distro(Some("ID=pop\nID_LIKE=\"ubuntu debian\"\n")),
        (Some(0), "debian".into())
    );
    assert_eq!(distro(Some("ID=fedora\n")).0, Some(10));
    assert_eq!(distro(None).0, Some(10));
    assert_eq!(distro(Some("ID=\"unterminated\n")).0, Some(10));
}

/// Test that status lines go to stdout only when the script runs under
/// the pack API. All other output goes to the log on stderr.
///   1. Run a script with `AIRLOCK_PACK_API=1` that prints a line and a
///      status
///   2. Check that stdout has only the status line and stderr has both
///   3. Run the script without the pack API and check that stdout is empty
#[test]
fn status_lines_reach_stdout_only_under_pack_api() {
    let api = run(
        Some("ID=alpine\n"),
        "echo plain\nairlock_status working hard\n",
        &[("AIRLOCK_PACK_API", "1")],
    );
    assert_eq!(api.code, Some(0), "{}", api.stderr);
    assert_eq!(api.stdout, "status working hard\n");
    assert!(api.stderr.contains("plain\n"));
    assert!(api.stderr.contains("airlock-pack: working hard\n"));

    let plain = run(Some("ID=alpine\n"), "airlock_status quiet\n", &[]);
    assert_eq!(plain.stdout, "");
    assert!(plain.stderr.contains("airlock-pack: quiet"));
}

/// Test that the helpers close fd 3 for the external commands they run.
/// Thus a package manager or vendor installer cannot write status lines.
///   1. Run each helper on Alpine or Debian with stubs that fail when fd 3
///      is open
///   2. Check that each run succeeds
///   3. Check that the Alpine package install calls `apk add`
///   4. Run a stub directly and check that it fails, which shows that the
///      stubs detect an open fd 3
#[test]
fn lib_helpers_close_fd_3_for_external_commands() {
    let cases = [
        ("alpine", "pkg_install pkg-a pkg-b"),
        ("alpine", "pkg_is_installed pkg-a || :"),
        // `_apt_ready=1` skips the apt setup, which writes to `/usr/sbin`
        // on the host.
        ("debian", "_apt_ready=1\npkg_install pkg-a"),
        ("debian", "_apt_ready=1\npkg_available pkg-a || :"),
        ("debian", "pkg_is_installed pkg-a || :"),
        ("debian", "dpkg_repair"),
        ("alpine", "fetch https://example.invalid/x \"$PACK_TMP/x\""),
        ("alpine", "run_vendor vendor vendor --flag"),
    ];
    for (id, body) in cases {
        let run = run(Some(&format!("ID={id}\n")), body, &[]);
        assert_eq!(run.code, Some(0), "{body}: {}", run.stderr);
        assert!(!run.stderr.contains("fd 3 is open"), "{body}");
    }
    let installed = run(Some("ID=alpine\n"), "pkg_install pkg-a", &[]);
    assert!(
        installed.stderr.contains("apk add --no-cache pkg-a"),
        "{}",
        installed.stderr
    );
    assert_eq!(run(Some("ID=alpine\n"), "vendor", &[]).code, Some(99));
}
