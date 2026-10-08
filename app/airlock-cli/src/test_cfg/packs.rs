//! Test packs, setup scripts run on the host, and an install exec that
//! runs the install scripts on the host in place of the install VM.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use airlock_test_utils::{TempDir, block_on, temp_dir};

use crate::config::{ConfigOverrides, LayeredConfig, ResolvedConfig};
use crate::packs::install::compose;
use crate::packs::install::progress::InstallProgress;
use crate::packs::install::setup::{Ended, Exec};
use crate::packs::install::state::{self, InstallState, ReadState};
use crate::packs::{InstallerScript, PackManager};
use crate::runtime::OutputSink;
use crate::util::PinnedDir;

/// The setup script of the test tool packs: two numbered steps, the
/// args on the log, and `mode = "exit-<n>"` exits with `<n>`.
pub const TOOL_SETUP: &str = r#"airlock_steps 2
airlock_status "preparing $AIRLOCK_PACK_ID"
echo "mode=$AIRLOCK_PACK_ARG_MODE fast-path=$AIRLOCK_PACK_ARG_FAST_PATH argv0=$0"
airlock_status installing
case "$AIRLOCK_PACK_ARG_MODE" in
exit-*) exit "${AIRLOCK_PACK_ARG_MODE#exit-}" ;;
esac
"#;

fn tool_pack(label: &str) -> String {
    format!(
        r#"label = "{label}"
description = "Installs {label}"
kind = "tool"

[[args]]
key = "mode"
type = "choice"
description = "Mode"
default = "fast"
values = ["fast", "slow"]
other = true

[[args]]
key = "fast-path"
type = "bool"
description = "Fast path"
default = false
"#
    )
}

/// The files of the test packs: the tools `alpha@1`, `alpha@2`, `beta@1`
/// and `gamma@1` with [`TOOL_SETUP`], and `plain@1`, a tool with a
/// static config and no setup script.
pub fn test_pack_files() -> Vec<(&'static str, String)> {
    let mut files = Vec::new();
    for (folder, label) in [
        ("alpha@1", "Alpha"),
        ("alpha@2", "Alpha"),
        ("beta@1", "Beta"),
        ("gamma@1", "Gamma"),
    ] {
        let pack: &'static str = format!("{folder}/pack.toml").leak();
        let setup: &'static str = format!("{folder}/setup.sh").leak();
        files.push((pack, tool_pack(label)));
        files.push((setup, TOOL_SETUP.to_string()));
    }
    files.push((
        "plain@1/pack.toml",
        "label = \"Plain\"\ndescription = \"Sets PLAIN\"\nkind = \"tool\"\n".to_string(),
    ));
    files.push(("plain@1/config.toml", "[env]\nPLAIN = \"1\"\n".to_string()));
    files
}

/// The packs of [`test_pack_files`]. Loaded once per test process.
pub fn test_packs() -> PackManager {
    static PACKS: LazyLock<PackManager> = LazyLock::new(|| load_packs(test_pack_files()));
    PACKS.clone()
}

/// The packs of the in-memory `packs/` directory `files`.
pub fn load_packs(files: Vec<(&'static str, String)>) -> PackManager {
    crate::packs::load_test_packs(files).expect("test packs load")
}

/// The config of one project file `airlock.toml` with `toml`, resolved
/// against `packs`.
pub fn resolve_with(packs: &PackManager, toml: &str) -> anyhow::Result<ResolvedConfig> {
    let layers =
        LayeredConfig::from_values(vec![], None, vec![("airlock.toml", toml::from_str(toml)?)])?;
    block_on(layers.resolve(packs, &ConfigOverrides::default()))
}

/// The installs of the configured packs of `toml` (resolved against
/// [`test_packs`]), in pack order.
pub fn test_installers(toml: &str) -> Vec<InstallerScript> {
    let resolved = resolve_with(&test_packs(), toml).unwrap();
    crate::start::install::install_candidates(&resolved.packs)
}

/// The shell that runs install scripts on the host: `dash` when there is
/// one (the strictest POSIX sh at hand), else `/bin/sh`.
pub fn sh() -> &'static str {
    if Path::new("/usr/bin/dash").exists() {
        "/usr/bin/dash"
    } else {
        "/bin/sh"
    }
}

/// The output of a script run by [`HostShell`].
pub struct ShellRun {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// A temp dir where install scripts run on the host: every external
/// command that `lib.sh` runs is a stub first on `PATH` (each fails with
/// 99 when fd 3 is open), an `os-release` file of its own, `HOME` and
/// `TMPDIR` in the temp dir, and no package-manager probe.
pub struct HostShell {
    dir: TempDir,
    bin: PathBuf,
}

impl HostShell {
    /// A shell whose os-release file has `os_release` (`None`: no file).
    pub fn new(os_release: Option<&str>) -> Self {
        let dir = temp_dir();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in [
            "apt-get",
            "apt-cache",
            "dpkg",
            "dpkg-query",
            "curl",
            "vendor",
        ] {
            stub(&bin, name, &format!("echo {name} \"$@\""));
        }
        stub(&bin, "apk", r#"echo apk "$@"; [ "$1" != info ]"#);
        stub(&bin, "uname", "echo x86_64");
        stub(&bin, "getconf", "exit 1");
        if let Some(content) = os_release {
            std::fs::write(dir.path().join("os-release"), content).unwrap();
        }
        Self { dir, bin }
    }

    /// The command of `argv` in this shell's environment.
    pub fn command(&self, argv: &[String]) -> Command {
        let mut command = Command::new(sh());
        command
            .args(&argv[1..])
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("HOME", self.dir.path())
            .env("TMPDIR", self.dir.path())
            .env("AIRLOCK_OS_RELEASE", self.dir.path().join("os-release"))
            .env("AIRLOCK_PKG_PROBE", "0");
        command
    }

    /// Run `script` with `sh -c` and the extra `env`.
    pub fn run(&self, script: &str, env: &[(&str, &str)]) -> ShellRun {
        let argv = ["sh".to_string(), "-c".to_string(), script.to_string()];
        let out = self
            .command(&argv)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        ShellRun {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

/// A stub command `name` in `bin`: fails with 99 when fd 3 is open, else
/// runs `action`.
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

/// An [`Exec`] that runs each install script on the host in a
/// [`HostShell`] (Alpine os-release) with the install's argv and env, and
/// feeds its output to the progress.
pub struct HostExec {
    shell: HostShell,
    /// Packs whose exec does not run the script: `None` did not start
    /// (Ctrl+C came first), `Some` ended so.
    stops: HashMap<String, Option<Ended>>,
    /// The sandbox directory whose install state is recorded as it is when
    /// each exec starts.
    state_dir: Option<PathBuf>,
    /// The packs whose exec started, in order.
    pub ran: Vec<String>,
    /// The progress message after each status-channel line.
    pub messages: Vec<String>,
    /// The install state on disk when each exec started (see
    /// [`Self::recording_state_of`]).
    pub on_disk: Vec<InstallState>,
}

impl HostExec {
    pub fn new() -> Self {
        Self {
            shell: HostShell::new(Some("ID=alpine\n")),
            stops: HashMap::new(),
            state_dir: None,
            ran: vec![],
            messages: vec![],
            on_disk: vec![],
        }
    }

    /// The exec of pack `id` ends with `ended` without running the script
    /// (`None`: it does not start).
    #[must_use]
    pub fn stopping(mut self, id: &str, ended: Option<Ended>) -> Self {
        self.stops.insert(id.to_string(), ended);
        self
    }

    /// Record the install state in `sandbox_dir` when each exec starts.
    #[must_use]
    pub fn recording_state_of(mut self, sandbox_dir: &Path) -> Self {
        self.state_dir = Some(sandbox_dir.to_path_buf());
        self
    }
}

impl Default for HostExec {
    fn default() -> Self {
        Self::new()
    }
}

impl Exec for HostExec {
    async fn run(
        &mut self,
        installer: &InstallerScript,
        progress: &mut InstallProgress,
    ) -> Option<Ended> {
        let stop = self.stops.remove(&installer.pack);
        if let Some(None) = stop {
            return None;
        }
        self.ran.push(installer.pack.clone());
        if let Some(dir) = &self.state_dir {
            let dir = PinnedDir::open(dir, Path::new(""), false).unwrap();
            self.on_disk.push(match state::read(&dir) {
                ReadState::Ok(s) => s,
                _ => InstallState::default(),
            });
        }
        if let Some(stop) = stop {
            return stop;
        }
        let out = self
            .shell
            .command(&compose::argv(installer))
            .envs(installer.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        for line in out.stdout.split_inclusive(|b| *b == b'\n') {
            progress.stdout(line);
            self.messages.push(progress.message());
        }
        progress.stderr(&out.stderr);
        Some(Ended::Exited(out.status.code().unwrap_or(-1)))
    }
}
