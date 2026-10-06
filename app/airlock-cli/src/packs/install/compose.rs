//! The install script of one pack: a wrapper, `lib.sh`, then the pack's
//! `setup.sh`, run with `/bin/sh -c` in the install boot.
//!
//! Protocol v1 (`AIRLOCK_PACK_API=1`): the wrapper moves the exec's stdout
//! to fd 3 and sends everything else to stderr (`exec 3>&1 1>&2`). So the
//! exec's stdout carries only status lines (`steps <n>` and
//! `status <text>`, written by `airlock_steps` and `airlock_status`; see
//! [`super::progress`]), and its stderr is the install log.

use crate::packs::InstallerScript;

/// The shared helpers of every install script.
const LIB: &str = include_str!("lib.sh");

/// Moves stdout to fd 3 (the status channel) and fd 1 to stderr.
const WRAPPER: &str = "exec 3>&1 1>&2\n";

/// The whole install script of the setup script `setup`: wrapper,
/// `lib.sh`, `setup`.
pub(in crate::packs) fn script(setup: &str) -> String {
    format!("{WRAPPER}{LIB}\n{setup}")
}

/// The argv of the install exec of `installer`.
pub fn argv(installer: &InstallerScript) -> Vec<String> {
    vec![
        "/bin/sh".into(),
        "-c".into(),
        installer.script.clone(),
        format!("airlock-pack-{}", installer.pack),
    ]
}

/// What an install script exit code means, for the failure message.
pub fn exit_hint(code: i32) -> Option<&'static str> {
    match code {
        10 => Some("the image is not Alpine- or Debian-based, or the CPU is not supported"),
        11 => Some("a package could not be installed; see the log"),
        12 => Some(
            "a download failed; check the network (GitHub rate limits show as HTTP 403) \
             and run `airlock start` again",
        ),
        13 => Some("an arg value of the pack is not valid; check the [packs] args"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::packs::{
        ArgKind, ArgValue, ConfiguredPack, Pack, PackArg, PackKind, PackMetadata, PackVersionData,
    };
    use crate::test_support::TempDir;

    /// The setup scripts of every version of the built-in packs, with
    /// the pack name.
    fn setups() -> Vec<(String, &'static str)> {
        crate::packs::init()
            .unwrap()
            .packs
            .iter()
            .filter_map(|p| Some((p.metadata().name.clone(), p.0.setup?)))
            .collect()
    }

    fn sh() -> &'static str {
        if Path::new("/usr/bin/dash").exists() {
            "/usr/bin/dash"
        } else {
            "/bin/sh"
        }
    }

    #[test]
    fn composed_scripts_pass_sh_syntax_check() {
        for (name, setup) in setups() {
            let script = script(setup);
            assert!(script.starts_with(WRAPPER));
            let out = Command::new(sh())
                .args(["-n", "-c", &script])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{name}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn only_the_mise_pack_uses_mise() {
        assert!(!LIB.contains("mise"));
        for (name, setup) in setups() {
            if name != "mise" {
                assert!(!setup.contains("mise"), "{name} mentions mise");
            }
        }
    }

    /// A stub command in `bin`: fails with 99 when fd 3 is open, else runs
    /// `action` (e.g. `echo "$@"; exit 1`).
    fn stub(bin: &Path, name: &str, action: &str) {
        let path = bin.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif ( : >&3 ) 2>/dev/null; then echo \"{name}: fd 3 is open\" >&2; \
                 exit 99; fi\n{action}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Every external command that `lib.sh` helpers run is stubbed here,
    /// so no test runs a real package manager or download on the host.
    fn stubs(bin: &Path) {
        for name in [
            "apt-get",
            "apt-cache",
            "dpkg",
            "dpkg-query",
            "curl",
            "vendor",
        ] {
            stub(bin, name, &format!("echo {name} \"$@\""));
        }
        // No package is installed yet.
        stub(bin, "apk", r#"echo apk "$@"; [ "$1" != info ]"#);
        stub(bin, "uname", "echo x86_64");
        stub(bin, "getconf", "exit 1");
    }

    struct Run {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    }

    /// Run the wrapper, `lib.sh` and `body` on the host with the stubs
    /// first on PATH, `os_release` as the os-release file, the
    /// package-manager probe off and the scratch space in the temp dir.
    /// `extra_env` is added to the environment.
    fn run_lib(os_release: Option<&str>, body: &str, extra_env: &[(&str, &str)]) -> Run {
        let tmp = TempDir::new("packs-lib");
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        stubs(&bin);
        let os = tmp.path().join("os-release");
        if let Some(content) = os_release {
            std::fs::write(&os, content).unwrap();
        }
        let path = format!("{}:/usr/bin:/bin", bin.display());
        let script = format!("{WRAPPER}{LIB}\n{body}");
        let out = Command::new(sh())
            .args(["-c", &script])
            .env_clear()
            .env("PATH", path)
            .env("HOME", tmp.path())
            .env("TMPDIR", tmp.path())
            .env("AIRLOCK_OS_RELEASE", &os)
            .env("AIRLOCK_PKG_PROBE", "0")
            .envs(extra_env.iter().copied())
            .output()
            .unwrap();
        Run {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn distro(os_release: Option<&str>) -> (Option<i32>, String) {
        let run = run_lib(os_release, "echo \"$DISTRO\"", &[]);
        // The wrapper moved stdout to stderr.
        let last = run.stderr.lines().last().unwrap_or_default().to_string();
        (run.code, last)
    }

    #[test]
    fn detect_distro_fixtures() {
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

    #[test]
    fn status_goes_to_stdout_and_the_rest_to_stderr() {
        let run = run_lib(
            Some("ID=alpine\n"),
            "echo plain\nairlock_status working hard\n",
            &[("AIRLOCK_PACK_API", "1")],
        );
        assert_eq!(run.code, Some(0), "{}", run.stderr);
        assert_eq!(run.stdout, "status working hard\n");
        assert!(run.stderr.contains("plain\n"));
        assert!(run.stderr.contains("airlock-pack: working hard\n"));

        // Without the API variable, status text only goes to the log.
        let run = run_lib(Some("ID=alpine\n"), "airlock_status quiet\n", &[]);
        assert_eq!(run.stdout, "");
        assert!(run.stderr.contains("airlock-pack: quiet"));
    }

    /// Every external command that a `lib.sh` helper runs gets `3>&-`:
    /// the stubs fail with 99 when fd 3 is still open.
    #[test]
    fn lib_helpers_close_fd_3_for_external_commands() {
        let cases = [
            ("alpine", "pkg_install pkg-a pkg-b"),
            ("alpine", "pkg_is_installed pkg-a || :"),
            ("debian", "_apt_ready=1\npkg_install pkg-a"),
            ("debian", "_apt_ready=1\npkg_available pkg-a || :"),
            ("debian", "pkg_is_installed pkg-a || :"),
            ("debian", "dpkg_repair"),
            ("alpine", "fetch https://example.invalid/x \"$PACK_TMP/x\""),
            ("alpine", "run_vendor vendor vendor --flag"),
        ];
        for (id, body) in cases {
            let run = run_lib(Some(&format!("ID={id}\n")), body, &[]);
            assert_eq!(run.code, Some(0), "{body}: {}", run.stderr);
            assert!(!run.stderr.contains("fd 3 is open"), "{body}");
        }
        // The stubs ran (the helpers really called them).
        let run = run_lib(Some("ID=alpine\n"), "pkg_install pkg-a", &[]);
        assert!(
            run.stderr.contains("apk add --no-cache pkg-a"),
            "{}",
            run.stderr
        );
        // A stub with fd 3 open fails: the check works.
        let run = run_lib(Some("ID=alpine\n"), "vendor", &[]);
        assert_eq!(run.code, Some(99));
    }

    #[test]
    fn exit_hints() {
        for code in [10, 11, 12, 13] {
            assert!(exit_hint(code).is_some());
        }
        assert!(exit_hint(1).is_none());
    }

    /// A pack `name` at `version` with the setup script `script`, a
    /// choice arg `mode` and a bool arg `use-login`.
    #[derive(Clone, Copy)]
    struct Fake {
        name: &'static str,
        version: &'static str,
        script: &'static str,
    }

    fn fake_args() -> Vec<PackArg> {
        vec![
            PackArg {
                key: "mode".into(),
                description: "Mode".into(),
                kind: ArgKind::Choice {
                    values: vec!["a".into(), "b".into()],
                    other: false,
                },
                default: ArgValue::Text("a".into()),
            },
            PackArg {
                key: "use-login".into(),
                description: "Login".into(),
                kind: ArgKind::Bool,
                default: ArgValue::Bool(false),
            },
        ]
    }

    /// `fake` configured with the arg values of the entry `entry`.
    fn configured(fake: Fake, entry: serde_json::Value) -> ConfiguredPack {
        let pack = Pack(std::sync::Arc::new(PackVersionData {
            metadata: PackMetadata {
                name: fake.name.into(),
                version: fake.version.into(),
                label: "Fake".into(),
                description: "Fake".into(),
                kind: PackKind::Tool,
                has_setup: true,
            },
            args: fake_args(),
            config: None,
            setup: Some(fake.script),
        }));
        let serde_json::Value::Object(entry) = entry else {
            panic!("an entry is a table");
        };
        let args: BTreeMap<String, ArgValue> = entry
            .into_iter()
            .map(|(key, raw)| {
                let arg = pack.args().iter().find(|a| a.key == key).unwrap();
                let value = arg.kind.parse(&raw).unwrap();
                (key, value)
            })
            .collect();
        pack.configure(&args)
    }

    fn fingerprint(configured: &ConfiguredPack) -> String {
        configured.setup_installer().unwrap().fingerprint
    }

    fn fake() -> Fake {
        Fake {
            name: "fake",
            version: "1",
            script: "echo hi\n",
        }
    }

    /// The fingerprint is the definition: name, version and arg values;
    /// the script does not count.
    #[test]
    fn fingerprint_tracks_the_definition_only() {
        let base = fingerprint(&configured(fake(), serde_json::json!({})));
        assert_eq!(base.len(), 64);
        assert_eq!(
            base,
            fingerprint(&configured(fake(), serde_json::json!({})))
        );
        let changed = [
            configured(
                Fake {
                    version: "2",
                    ..fake()
                },
                serde_json::json!({}),
            ),
            configured(
                Fake {
                    name: "other",
                    ..fake()
                },
                serde_json::json!({}),
            ),
            configured(fake(), serde_json::json!({ "mode": "b" })),
            configured(fake(), serde_json::json!({ "use-login": true })),
        ];
        for c in changed {
            assert_ne!(fingerprint(&c), base);
        }
        // Another script, a default written out, and key order.
        assert_eq!(
            fingerprint(&configured(
                Fake {
                    script: "echo bye\n",
                    ..fake()
                },
                serde_json::json!({})
            )),
            base
        );
        assert_eq!(
            fingerprint(&configured(fake(), serde_json::json!({ "mode": "a" }))),
            base
        );
        let json_ab: serde_json::Value =
            serde_json::from_str(r#"{"mode": "b", "use-login": true}"#).unwrap();
        let json_ba: serde_json::Value =
            serde_json::from_str(r#"{"use-login": true, "mode": "b"}"#).unwrap();
        assert_eq!(
            fingerprint(&configured(fake(), json_ab)),
            fingerprint(&configured(fake(), json_ba))
        );
    }

    #[test]
    fn env_has_protocol_id_and_args() {
        let c = configured(fake(), serde_json::json!({ "mode": "b" }))
            .setup_installer()
            .unwrap();
        assert_eq!(
            c.env,
            [
                ("AIRLOCK_PACK_API".to_string(), "1".to_string()),
                ("AIRLOCK_PACK_ID".to_string(), "fake".to_string()),
                ("AIRLOCK_PACK_ARG_MODE".to_string(), "b".to_string()),
                (
                    "AIRLOCK_PACK_ARG_USE_LOGIN".to_string(),
                    "false".to_string()
                ),
            ]
        );
        let argv = argv(&c);
        assert_eq!(argv[..2], ["/bin/sh", "-c"]);
        assert!(argv[2].ends_with("echo hi\n"));
        assert_eq!(argv[3], "airlock-pack-fake");
    }
}
