//! Shell completions.
//!
//! Prints the shell script that registers the airlock completions, and adds
//! a line that loads this script to the startup file of the shell. The
//! shell calls airlock for each completion request. Thus the completions
//! know the commands and options, and also the names of the templates and
//! the ids of the sandboxes of the user.
//!
//! The value completers run in the completion request. They must be fast,
//! must not write output and must not change files. On an error they give
//! no values.

use std::ffi::OsString;
use std::io::Write as _;

use clap::{Args, Subcommand, ValueEnum};
use clap_complete::CompletionCandidate;
use clap_complete::env::Shells;

use crate::data_dir::DataDir;
use crate::settings::Settings;
use crate::util::VerifiedPath;
use crate::{cli, messages, sandbox, templates};

/// Environment variable that tells airlock to answer a completion request
/// for the shell in its value.
pub const ENV_VAR: &str = "AIRLOCK_COMPLETE";

/// Start of the zsh script: run `compinit` if the user did not run it.
const ZSH_COMPINIT: &str = "(( $+functions[compdef] )) || { autoload -Uz compinit && compinit; }\n";

/// Max size of the startup file that `install` reads.
const MAX_RC_FILE_SIZE: u64 = 4 * 1024 * 1024;

/// Arguments of `airlock completions`.
#[derive(Args, Debug)]
pub struct CompletionsArgs {
    #[command(subcommand)]
    cmd: CompletionsCmd,
}

/// Subcommands of `airlock completions`.
#[derive(Subcommand, Debug)]
enum CompletionsCmd {
    /// Print the completion script of a shell, for use with eval
    Script {
        /// Shell of the script
        shell: Shell,
    },
    /// Add the completions to the startup file of a shell
    Install {
        /// Shell to install for (default: the shell in $SHELL)
        shell: Option<Shell>,
    },
}

/// Shells with completion support.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    /// Get the shell from a shell path, for example the value of `$SHELL`.
    fn from_path(path: &str) -> Option<Self> {
        let name = path.rsplit('/').next()?;
        Self::from_str(name, false).ok()
    }

    fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
        }
    }

    /// Get the line that loads the completions in the startup file. It does
    /// nothing if airlock is not in `PATH`, so that a removed airlock does
    /// not break the shell start.
    fn rc_line(self) -> String {
        match self {
            Self::Bash | Self::Zsh => format!(
                "command -v airlock >/dev/null && eval \"$(airlock completions script {})\"",
                self.name()
            ),
            Self::Fish => {
                "command -q airlock; and airlock completions script fish | source".to_string()
            }
        }
    }
}

/// Entry point for `airlock completions`.
/// Returns:
///   Process exit code: 0 on success, 1 on error.
pub fn main(args: &CompletionsArgs) -> i32 {
    let result = match args.cmd {
        CompletionsCmd::Script { shell } => script(shell),
        CompletionsCmd::Install { shell } => install(shell),
    };
    result.unwrap_or_else(|e| {
        cli::error!("{e:#}");
        1
    })
}

/// Print the registration script of `shell` to stdout.
fn script(shell: Shell) -> anyhow::Result<i32> {
    let shells = Shells::builtins();
    let completer = shells
        .completer(shell.name())
        .ok_or_else(|| anyhow::anyhow!(messages::completions::unknown_shell(shell.name())))?;
    let mut buf = Vec::new();
    if shell == Shell::Zsh {
        // The zsh script registers with `compdef`, which exists only after
        // `compinit`. Without it, each new shell shows an error.
        buf.extend_from_slice(ZSH_COMPINIT.as_bytes());
    }
    completer.write_registration(ENV_VAR, "airlock", "airlock", &completer_path(), &mut buf)?;
    std::io::stdout().write_all(&buf)?;
    Ok(0)
}

/// Get the program that the shell calls for a completion request: the
/// program name as the user started it. A relative path becomes absolute,
/// so that the completions work in all directories. A bare name stays, and
/// the shell finds it in `PATH`.
fn completer_path() -> String {
    let argv0 = std::env::args_os()
        .next()
        .unwrap_or_else(|| OsString::from("airlock"));
    let argv0 = argv0.to_string_lossy().into_owned();
    if argv0.contains('/')
        && !argv0.starts_with('/')
        && let Ok(cwd) = VerifiedPath::current_dir()
    {
        return format!("{}/{argv0}", cwd.given().display());
    }
    argv0
}

/// Add the line that loads the completions to the startup file of `shell`.
/// Does nothing if the file loads the completions already, also in a form
/// that the user wrote.
fn install(shell: Option<Shell>) -> anyhow::Result<i32> {
    let shell = if let Some(shell) = shell {
        shell
    } else {
        let path = std::env::var("SHELL").unwrap_or_default();
        Shell::from_path(&path)
            .ok_or_else(|| anyhow::anyhow!(messages::completions::unknown_shell(&path)))?
    };
    let rc = rc_file(shell)?;
    let line = shell.rc_line();
    let content = match rc.read_to_string_capped(MAX_RC_FILE_SIZE) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => anyhow::bail!(messages::completions::read_failed(&rc, e)),
    };
    if has_script_call(&content, shell) {
        cli::log!("{}", messages::completions::already_installed(&rc));
        return Ok(0);
    }
    append_line(&rc, &content, &line)
        .map_err(|e| anyhow::anyhow!(messages::completions::write_failed(&rc, e)))?;
    cli::log!("{}", messages::completions::installed(&rc));
    Ok(0)
}

/// Check if the startup file text `content` loads the completion script
/// of `shell`. The user can format the line in a different way, so only
/// the words of the command must match.
fn has_script_call(content: &str, shell: Shell) -> bool {
    let pattern = format!(r"\bairlock\s+completions\s+script\s+{}\b", shell.name());
    regex::Regex::new(&pattern).is_ok_and(|re| re.is_match(content))
}

/// Get the startup file of `shell`. The file can be a link into a dotfiles
/// repo, so links are followed.
fn rc_file(shell: Shell) -> anyhow::Result<VerifiedPath> {
    let home = VerifiedPath::home_dir()?;
    let rc = match shell {
        Shell::Bash => home.join_follow(".bashrc", None)?,
        Shell::Zsh => match std::env::var_os("ZDOTDIR").filter(|d| !d.is_empty()) {
            Some(dir) => VerifiedPath::new_follow(dir, None)?.join_follow(".zshrc", None)?,
            None => home.join_follow(".zshrc", None)?,
        },
        // Fish loads this file when the user completes an airlock command.
        Shell::Fish => match std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
            Some(dir) => VerifiedPath::new_follow(dir, None)?
                .join_follow("fish/completions/airlock.fish", None)?,
            None => home.join_follow(".config/fish/completions/airlock.fish", None)?,
        },
    };
    Ok(rc)
}

/// Append `line` to the file `rc` with the current text `content`. Makes
/// the file and its parent directories if they do not exist.
fn append_line(rc: &VerifiedPath, content: &str, line: &str) -> std::io::Result<()> {
    if let Some(parent) = rc.parent() {
        parent.mkdirs()?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    let mut file = rc.open_with(&opts)?;
    // Keep the line apart from the last line of the file.
    let sep = match content {
        "" => "",
        c if c.ends_with('\n') => "\n",
        _ => "\n\n",
    };
    write!(file, "{sep}{}\n{line}\n", messages::completions::RC_COMMENT)
}

// -- Value completers --

/// Complete the names of the templates.
pub fn template_names() -> Vec<CompletionCandidate> {
    let Some(data) = data_dir() else {
        return Vec::new();
    };
    templates::list(&data)
        .into_iter()
        .map(|t| CompletionCandidate::new(t.name))
        .collect()
}

/// Complete the ids of the sandboxes. The help of each id is its project
/// directory.
pub fn sandbox_ids() -> Vec<CompletionCandidate> {
    data_dir().map(read_sandbox_ids).unwrap_or_default()
}

/// Read the ids and project directories of the sandboxes in `data`.
fn read_sandbox_ids(data: DataDir) -> Vec<CompletionCandidate> {
    if !data.db.is_dir() {
        return Vec::new();
    }
    // The completion request runs in the async main. A nested runtime is
    // not allowed in a runtime thread, thus the read has its own thread.
    let entries = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async {
            let db = crate::db::Db::open(&data.db).ok()?;
            sandbox::list_entries(&db).await.ok()
        })
    })
    .join()
    .ok()
    .flatten()
    .unwrap_or_default();
    entries
        .into_iter()
        .map(|e| {
            let project = e.project.to_string_lossy().into_owned();
            CompletionCandidate::new(e.id).help(Some(project.into()))
        })
        .collect()
}

/// Get the airlock data directory of the user, if it exists. A completion
/// request must not make it.
fn data_dir() -> Option<DataDir> {
    let root = Settings::load().ok()?.data_dir().ok()?;
    if !root.is_dir() {
        return None;
    }
    DataDir::init(&root).ok()
}

#[cfg(test)]
mod tests {
    use airlock_test_utils::temp_dir;

    use super::*;
    use crate::db::Db;
    use crate::sandbox::registry;

    /// Test that the sandbox id completer lists the registered sandboxes
    /// with their project directories, and that it does not write to the
    /// database. The completer runs in the async main, so the test runs it
    /// in a runtime too.
    ///   1. Make an empty database and run the completer
    ///   2. Check that there are no candidates and no new sandbox table
    ///   3. Register a project and run the completer again
    ///   4. Check that the candidate is the id with the project as help
    #[tokio::test]
    async fn sandbox_ids_in_runtime_list_registered_sandboxes() {
        let home = temp_dir();
        let home_path = std::fs::canonicalize(home.path()).unwrap();
        let root = VerifiedPath::new_follow(&home_path, None).unwrap();
        let data = DataDir::init(&root).unwrap();
        drop(Db::open(&data.db).unwrap());

        assert!(read_sandbox_ids(data.clone()).is_empty());
        assert!(!Db::open(&data.db).unwrap().has_database("sandboxes"));

        let project = root.join("project").unwrap();
        let id = {
            let db = Db::open(&data.db).unwrap();
            registry::find_or_register(&db, &data.sandboxes, &project)
                .await
                .unwrap()
        };

        let candidates = read_sandbox_ids(data);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].get_value(), id.as_str());
        let help = candidates[0].get_help().unwrap().to_string();
        assert_eq!(help, project.given().to_string_lossy());
    }
}
