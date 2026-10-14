//! The config change check of `airlock start`.
//!
//! Keeps the config files of the last run in the sandbox. If the final config
//! of a new run is different from the final config of the last run, the user
//! must answer before the sandbox starts. The user can see the changes, and
//! abort (the default), accept them, or continue with the config of the
//! last run. Thus
//! code in the sandbox cannot quietly change the config of the next run, for
//! example with an edit of the project config file.
//!
//! The check also compares the host paths of the config (mount sources, the
//! custom kernel and initramfs, and host sockets) after their symlinks are
//! resolved. Thus a symlink that points to an other place since the last
//! run also gives the question, also when the config text is the same.
//!
//! A sandbox without a copy (a new sandbox, or a sandbox of an older airlock)
//! gets no question. The run keeps a copy for the next run. A copy of an
//! older airlock without host paths gives no question about host paths.
//!
//! The diff does not show the real values of masked `[env]` entries. A hash
//! of each value replaces it.

use console::Style;
use serde_json::Value;
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};

use super::Exit;
use crate::cli::prompt::choose::{Choice, Choose, Details};
use crate::cli::prompt::style::Styles;
use crate::config::config_values::ConfigValues;
use crate::config::export::{self, Format};
use crate::config::{ConfigOverrides, LayeredConfig, ResolvedConfig};
use crate::data_dir::DataDir;
use crate::packs::PackManager;
use crate::project::{HostPath, HostPaths};
use crate::util::VerifiedPath;
use crate::{messages, project};

/// The key that shows and hides the changes.
const DIFF_KEY: char = 's';
/// Number of unchanged lines around each change in the diff.
const DIFF_CONTEXT: usize = 3;

/// The config of the run, and how to find the config of the last run.
pub struct CheckInput<'a> {
    /// Sandbox directory with the config of the last run
    pub sandbox_dir: &'a VerifiedPath,
    /// Resolved config of this run
    pub resolved: &'a ResolvedConfig,
    /// Resolved host paths of this run (see [`host_paths`])
    pub paths: &'a HostPaths,
    /// Airlock data directory, to resolve the config of the last run
    pub data_dir: &'a DataDir,
    /// Known packs, to resolve the config of the last run
    pub packs: &'a PackManager,
    /// Command line overrides of this run. They also apply to the config of
    /// the last run.
    pub overrides: &'a ConfigOverrides,
    /// A terminal is available for questions
    pub can_prompt: bool,
    /// `--yes`: use the default answer (abort) without a question
    pub yes: bool,
    /// `--allow-configuration-changes`: accept the changes without a
    /// question
    pub allow_changes: bool,
}

/// The config that the run uses.
pub enum Checked {
    /// The config of this run. Save it after the sandbox step (see
    /// [`save`]).
    Current,
    /// The config of the last run. Do not save it, so that the next run asks
    /// again.
    Last(Box<ResolvedConfig>),
}

/// Check the final config of this run against the config of the last run.
///
/// If the config changed, the user must abort, accept the changes, or
/// continue with the config of the last run. `--allow-configuration-changes`
/// accepts the changes without a question.
/// Returns:
///   The config to use. Exit with code 2 for a change without a terminal or
///   with `--yes`. Abort and Esc end the run.
pub async fn check(input: &CheckInput<'_>) -> Result<Checked, Exit> {
    check_with(input, ask).await
}

/// [`check`] with the question `ask` (see [`ask`] for its args).
async fn check_with(
    input: &CheckInput<'_>,
    ask: impl FnOnce(&[String], Option<&str>) -> Result<Answer, Exit>,
) -> Result<Checked, Exit> {
    let meta = project::read_run_meta(input.sandbox_dir);
    let Some(stored) = meta.last_run_config else {
        return Ok(Checked::Current);
    };
    let last = resolve_last(input, stored).await;
    let current = final_values(input.resolved)?;
    let styles = Styles::new();
    // A `run.json` of an older airlock has no paths: no path change.
    let moved = meta
        .last_run_paths
        .map(|last| path_diff(&last, input.paths, &styles))
        .unwrap_or_default();
    if let Ok((_, values)) = &last
        && *values == current
        && moved.is_empty()
    {
        return Ok(Checked::Current);
    }
    if input.allow_changes {
        return Ok(Checked::Current);
    }
    // `--yes` selects the default answer, which is abort. A script must not
    // accept a change that code in the sandbox can make.
    if input.yes || !input.can_prompt {
        return Err(Exit::error(2, messages::start::CONFIG_CHANGED_NO_PROMPT));
    }
    match last {
        Ok((resolved, values)) => {
            let mut diff = diff(&to_toml(&values), &to_toml(&current), &styles);
            diff.extend(moved);
            match ask(&diff, None)? {
                Answer::Accept => Ok(Checked::Current),
                Answer::Last => Ok(Checked::Last(Box::new(resolved))),
            }
        }
        Err(e) => {
            let note = messages::start::last_config_invalid(e);
            let mut diff = diff("", &to_toml(&current), &styles);
            diff.extend(moved);
            ask(&diff, Some(&note))?;
            Ok(Checked::Current)
        }
    }
}

/// Save the config files `config` and their resolved host paths `paths` as
/// the config of the last run in the sandbox directory `sandbox_dir`.
pub fn save(
    sandbox_dir: &VerifiedPath,
    config: &LayeredConfig,
    paths: &HostPaths,
) -> anyhow::Result<()> {
    project::save_last_run_config(sandbox_dir, serde_json::to_value(config)?, paths.clone())
}

/// Resolve the host paths of `values`: the sources of the enabled mounts,
/// `vm.kernel`, `vm.initramfs` and the host sockets of the enabled socket
/// forwards. Creates nothing.
/// Args:
///  - `home`: Home dir of the user. It expands `~`
///  - `cwd`: Project directory. Relative paths resolve from it
///
/// Returns:
///   The paths by config key. A path that does not resolve keeps its
///   absolute form.
pub fn host_paths(values: &ConfigValues, home: &VerifiedPath, cwd: &VerifiedPath) -> HostPaths {
    let mounts = values
        .mounts
        .iter()
        .filter(|(_, m)| m.enabled)
        .map(|(name, m)| (format!("mounts.{name}.source"), &m.source));
    let assets = [
        ("vm.kernel", &values.vm.kernel),
        ("vm.initramfs", &values.vm.initramfs),
    ]
    .into_iter()
    .filter_map(|(key, path)| Some((key.to_string(), path.as_ref()?)));
    let sockets = values
        .network
        .sockets
        .iter()
        .filter(|(_, s)| s.enabled)
        .map(|(name, s)| (format!("network.sockets.{name}.host"), &s.host.source));
    // Fallback base for `~` when a path does not resolve.
    let home_text = home
        .to_entry_path()
        .map_or_else(|_| home.to_string(), |p| p.display().to_string());
    mounts
        .chain(assets)
        .chain(sockets)
        .map(|(key, given)| {
            let resolved = crate::vm::mount::host_path(given, home, cwd)
                .and_then(|p| Ok(p.to_entry_path()?.display().to_string()))
                .unwrap_or_else(|_| {
                    let path = crate::vm::mount::expand_tilde_str(given, &home_text);
                    if path.starts_with('/') {
                        path
                    } else {
                        format!("{cwd}/{path}")
                    }
                });
            (
                key,
                HostPath {
                    given: given.clone(),
                    resolved,
                },
            )
        })
        .collect()
}

/// Return the host paths that `new` resolves to an other place than
/// `last`, also the added and removed ones, as diff lines. Empty if no
/// path changed.
fn path_diff(last: &HostPaths, new: &HostPaths, styles: &Styles) -> Vec<String> {
    use messages::start as m;
    let line = |style: &Style, sign: char, key: &str, path: &HostPath, resolved: String| {
        let text = m::host_path_line(key, &path.given, resolved);
        style.apply_to(format!("{sign} {text}")).to_string()
    };
    let mut lines = Vec::new();
    for (key, old) in last {
        match new.get(key) {
            None => lines.push(line(&styles.red, '-', key, old, old.resolved.clone())),
            Some(path) if path.resolved != old.resolved => {
                let moved = m::host_path_moved(&old.resolved, &path.resolved);
                lines.push(line(&styles.cyan, '~', key, path, moved));
            }
            Some(_) => {}
        }
    }
    for (key, path) in new.iter().filter(|(key, _)| !last.contains_key(*key)) {
        lines.push(line(&styles.green, '+', key, path, path.resolved.clone()));
    }
    if !lines.is_empty() {
        lines.insert(0, String::new());
        lines.insert(1, styles.dim.apply_to(m::HOST_PATHS_CHANGED).to_string());
    }
    lines
}

/// Resolve the stored config files of the last run.
/// Returns:
///   The resolved config and its final values (see [`final_values`]), or
///   an error if the files do not resolve any more.
async fn resolve_last(
    input: &CheckInput<'_>,
    stored: Value,
) -> anyhow::Result<(ResolvedConfig, Value)> {
    let config: LayeredConfig = serde_json::from_value(stored)?;
    let resolved = config
        .resolve(input.data_dir, input.packs, input.overrides)
        .await?;
    let values = final_values(&resolved)?;
    Ok((resolved, values))
}

/// Return the final config values of `resolved` for the comparison and the
/// diff. The `--network` policy is not in them, because it applies to one
/// run only. Masked `[env]` values are replaced with a hash. The enabled
/// packs with their versions and args are in them.
fn final_values(resolved: &ResolvedConfig) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(&resolved.values)?;
    value["network"]["policy"] = serde_json::to_value(resolved.configured_policy)?;
    // A pack can have no config values. Without the pack list, a change of
    // such a pack is not a config change.
    let mut packs = serde_json::Map::new();
    for pack in &resolved.packs {
        let metadata = pack.metadata();
        let mut entry = serde_json::Map::new();
        entry.insert("version".into(), metadata.version.clone().into());
        for (key, arg) in pack.args() {
            entry.insert(key.clone(), serde_json::to_value(arg)?);
        }
        packs.insert(metadata.name.clone(), entry.into());
    }
    value["packs"] = packs.into();
    if let Some(env) = value.get_mut("env").and_then(Value::as_object_mut) {
        for entry in env.values_mut() {
            hide_masked_value(entry);
        }
    }
    Ok(value)
}

/// Replace the value of a masked `[env]` entry with a hash of the value.
/// Thus a changed value still shows as a change.
fn hide_masked_value(entry: &mut Value) {
    if entry.get("mask").and_then(Value::as_bool) != Some(true) {
        return;
    }
    if let Some(value) = entry.get_mut("value")
        && let Some(text) = value.as_str()
    {
        let hash = hex::encode(Sha256::digest(text.as_bytes()));
        *value = messages::start::masked_value_hash(&hash[..16]).into();
    }
}

/// The answer to the config question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// Use the config of this run.
    Accept,
    /// Use the config of the last run.
    Last,
}

/// Ask if the user accepts the config changes. The first answer, `Abort`,
/// is the default.
/// Args:
///  - `diff`: Lines of the changes
///  - `invalid_last`: Note if the config of the last run does not resolve
///    any more. Then the question has no option for that config.
///
/// Returns:
///   The answer. Abort and Esc end the run.
fn ask(diff: &[String], invalid_last: Option<&str>) -> Result<Answer, Exit> {
    let show_hint = messages::start::diff_show_hint(DIFF_KEY);
    let hide_hint = messages::start::diff_hide_hint(DIFF_KEY);
    let mut answers = vec![(messages::start::CONFIG_CHOICE_ACCEPT, Answer::Accept)];
    if invalid_last.is_none() {
        answers.push((messages::start::CONFIG_CHOICE_LAST, Answer::Last));
    }
    let choices: Vec<Choice> = [messages::cli::ABORT_CHOICE]
        .into_iter()
        .chain(answers.iter().map(|(label, _)| *label))
        .map(|label| Choice { label })
        .collect();
    let notes: Vec<&str> = invalid_last.into_iter().collect();
    let question = Choose {
        title: messages::start::CONFIG_CHANGED_TITLE,
        notes: &notes,
        choices: &choices,
        default: 0,
        report: true,
    };
    let details = Details {
        key: DIFF_KEY,
        show_hint: &show_hint,
        hide_hint: &hide_hint,
        lines: diff,
    };
    match question.ask_with_details(&details)? {
        Some(i) if i > 0 => Ok(answers[i - 1].1),
        _ => Err(Exit::aborted()),
    }
}

/// Return `config` as TOML text, which is easier to read in a diff. Use
/// JSON if `config` has no TOML form.
fn to_toml(config: &Value) -> String {
    export::render(config, Format::Toml)
        .or_else(|_| export::render(config, Format::Json))
        .unwrap_or_default()
}

/// Return the line changes from `last` to `new` as unified diff lines.
/// Removed lines are red and added lines are green.
fn diff(last: &str, new: &str, styles: &Styles) -> Vec<String> {
    let diff = TextDiff::from_lines(last, new);
    let mut lines = Vec::new();
    for (i, group) in diff.grouped_ops(DIFF_CONTEXT).iter().enumerate() {
        if i > 0 {
            lines.push(styles.dim.apply_to("  …").to_string());
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let line = change.value().trim_end_matches('\n');
                let (style, sign) = match change.tag() {
                    ChangeTag::Delete => (&styles.red, '-'),
                    ChangeTag::Insert => (&styles.green, '+'),
                    ChangeTag::Equal => (&styles.dim, ' '),
                };
                lines.push(style.apply_to(format!("{sign} {line}")).to_string());
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    //! Tests of the final values and the diff.

    use airlock_test_utils::{block_on, temp_dir};

    use super::*;
    use crate::config::config_values::Policy;
    use crate::test_cfg::config::resolve_project_toml;

    /// What the config question got.
    struct Asked {
        /// The diff lines, without colors.
        diff: Vec<String>,
        /// The note about a config of the last run that is not valid.
        invalid_last: Option<String>,
    }

    /// The config files with one project file `toml`.
    fn layers(toml: &str) -> LayeredConfig {
        LayeredConfig::from_values(
            vec![],
            None,
            vec![("airlock.toml", toml::from_str(toml).unwrap())],
        )
        .unwrap()
    }

    /// Check the project file `toml` against the stored project file
    /// `last`, with a terminal. The question gives `answer`.
    /// Returns:
    ///   The check result, and what the question got, if it came.
    fn check_answer(
        last: &str,
        toml: &str,
        answer: Answer,
    ) -> (Result<Checked, Exit>, Option<Asked>) {
        check_paths_answer(last, toml, &HostPaths::new(), &HostPaths::new(), answer)
    }

    /// [`check_answer`] with the host paths `last_paths` of the last run
    /// and `paths` of this run.
    fn check_paths_answer(
        last: &str,
        toml: &str,
        last_paths: &HostPaths,
        paths: &HostPaths,
        answer: Answer,
    ) -> (Result<Checked, Exit>, Option<Asked>) {
        let sandbox = temp_dir();
        let sandbox_dir = VerifiedPath::new_follow(sandbox.path(), None).unwrap();
        let data = temp_dir();
        let data_dir = crate::test_cfg::oci::data_dir_at(data.path());
        let packs = crate::packs::init_with_sample();
        save(&sandbox_dir, &layers(last), last_paths).unwrap();
        let config = layers(toml);
        let resolved =
            block_on(config.resolve(&data_dir, &packs, &ConfigOverrides::default())).unwrap();
        let input = CheckInput {
            sandbox_dir: &sandbox_dir,
            resolved: &resolved,
            paths,
            data_dir: &data_dir,
            packs: &packs,
            overrides: &ConfigOverrides::default(),
            can_prompt: true,
            yes: false,
            allow_changes: false,
        };
        let mut asked = None;
        let checked = block_on(check_with(&input, |diff, invalid_last| {
            asked = Some(Asked {
                diff: diff
                    .iter()
                    .map(|line| console::strip_ansi_codes(line).into_owned())
                    .collect(),
                invalid_last: invalid_last.map(ToString::to_string),
            });
            Ok(answer)
        }));
        (checked, asked)
    }

    /// Test that the user can continue with the config of the last run, and
    /// that the same config gives no question.
    ///   1. Check a changed config and answer "continue with old
    ///      configuration"
    ///   2. Check that the question showed the diff and that the run gets
    ///      the config of the last run
    ///   3. Check the same config and check that no question came
    #[test]
    fn continue_with_old_configuration_gives_config_of_last_run() {
        let (checked, asked) = check_answer("[vm]\ncpus = 2\n", "[vm]\ncpus = 3\n", Answer::Last);
        let asked = asked.expect("a question");
        assert!(asked.diff.iter().any(|line| line == "- cpus = 2"));
        assert!(asked.invalid_last.is_none());
        let Ok(Checked::Last(last)) = checked else {
            panic!("not the config of the last run");
        };
        assert_eq!(last.values.vm.cpus, 2);

        let (checked, asked) = check_answer("[vm]\ncpus = 2\n", "[vm]\ncpus = 2\n", Answer::Last);
        assert!(matches!(checked, Ok(Checked::Current)));
        assert!(asked.is_none());
    }

    /// Test that a stored config that does not resolve any more gives the
    /// question with a note, and the run can accept the new config.
    ///   1. Store a config with an unknown preset name
    ///   2. Check a config and check that the question has the note
    ///   3. Accept and check that the run gets the new config
    #[test]
    fn stored_config_that_does_not_resolve_is_explained_in_question() {
        let (checked, asked) = check_answer(
            "presets = [\"no-such-preset\"]\n",
            "[vm]\ncpus = 3\n",
            Answer::Accept,
        );
        assert!(asked.expect("a question").invalid_last.is_some());
        assert!(matches!(checked, Ok(Checked::Current)));
    }

    /// Test that a mount source symlink that points to an other dir since
    /// the last run gives the question, and that the question shows the old
    /// and the new path. Code in the sandbox can change a symlink in the
    /// project dir.
    ///   1. Make the dirs `a` and `b` and the symlink `data` to `a`
    ///   2. Resolve the host paths, then change the symlink to `b`
    ///   3. Check the same config with the new paths
    ///   4. Check that the question came and shows both paths
    #[test]
    fn retargeted_mount_source_symlink_shows_old_and_new_path() {
        let dir = temp_dir();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("a")).unwrap();
        std::fs::create_dir(root.join("b")).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("data")).unwrap();
        let toml = "[mounts.data]\nsource = \"./data\"\ntarget = \"/data\"\n";
        let values = resolve_project_toml(toml).unwrap().values;
        let cwd = crate::test_cfg::oci::verified_dir(&root);
        let last = host_paths(&values, &cwd, &cwd);
        std::fs::remove_file(root.join("data")).unwrap();
        std::os::unix::fs::symlink(root.join("b"), root.join("data")).unwrap();
        let paths = host_paths(&values, &cwd, &cwd);

        let (checked, asked) = check_paths_answer(toml, toml, &last, &paths, Answer::Accept);
        assert!(matches!(checked, Ok(Checked::Current)));
        let line = format!(
            "~ mounts.data.source = \"./data\": {} → {}",
            root.join("a").display(),
            root.join("b").display()
        );
        let diff = asked.expect("a question").diff;
        assert!(diff.contains(&line), "{diff:?}");
    }

    /// The final values of the project file `toml`.
    fn values(toml: &str) -> Value {
        final_values(&resolve_project_toml(toml).unwrap()).unwrap()
    }

    /// Test that the `--network` policy is not part of the final values,
    /// because it applies to one run only.
    ///   1. Get the final values of a config
    ///   2. Get the final values of the same config with a different run
    ///      policy
    ///   3. Check that the values are the same
    #[test]
    fn network_override_is_not_config_change() {
        let config = "[vm]\ncpus = 2\n";
        let mut overridden = resolve_project_toml(config).unwrap();
        overridden.values.network.policy = Policy::AllowAlways;
        assert_eq!(values(config), final_values(&overridden).unwrap());
    }

    /// Test that a removed pack without config values changes the final
    /// values, so that the config question comes before the tool question.
    ///   1. Get the final values of a config with the mise pack, which has
    ///      no config values
    ///   2. Get the final values of the same config without the pack
    ///   3. Check that the values are different
    #[test]
    fn removed_pack_without_config_values_is_config_change() {
        assert_ne!(values("[packs]\nmise = { version = 1 }\n"), values(""));
    }

    /// Test that the final values do not show masked `[env]` values, but a
    /// changed masked value still changes them.
    ///   1. Get the final values of two configs with different masked values
    ///   2. Check that no values contain their real value
    ///   3. Check that the values are different
    #[test]
    fn masked_env_value_is_hidden_but_its_change_shows() {
        let text = |secret: &str| {
            let config = format!(
                "[env]\nTOKEN = {{ value = \"{secret}\", mask = true }}\nPLAIN = \"visible\"\n"
            );
            values(&config).to_string()
        };
        let first = text("secret-one");
        let second = text("secret-two");
        assert!(!first.contains("secret-one"));
        assert!(!second.contains("secret-two"));
        assert!(first.contains("visible"));
        assert_ne!(first, second);
    }

    /// Test that the diff shows the changed values of two configs as TOML
    /// lines, with context.
    ///   1. Make the diff of the TOML forms of two configs with one changed
    ///      value
    ///   2. Check the removed and added lines and the context
    #[test]
    fn diff_shows_changed_config_values_as_toml() {
        let lines: Vec<String> = diff(
            &to_toml(&values("[vm]\ncpus = 2\n")),
            &to_toml(&values("[vm]\ncpus = 3\n")),
            &Styles::new(),
        )
        .iter()
        .map(|line| console::strip_ansi_codes(line).into_owned())
        .collect();
        let changed: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.starts_with("  "))
            .collect();
        assert_eq!(changed, ["- cpus = 2", "+ cpus = 3"]);
        assert!(lines.len() > changed.len(), "{lines:?}");
    }
}
