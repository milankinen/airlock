//! The sandbox step of `airlock start`: the stored sandbox (image, disk,
//! install records) and the decisions about its tools.
//!
//! Four questions can come up for an existing sandbox, each with
//! `re-create sandbox` as the default: a changed image, changed packs
//! (whose definition differs from the one on the disk; the only other
//! answer is `cancel`), tools removed from the config, and tools added to
//! it. `--yes` picks the default without a question. Without a terminal and
//! without `--yes` a needed question is an error (exit code 2); a tool
//! question fails before the image pull. A new disk, an unchanged sandbox
//! and the retry of an unfinished install (see [`Why::Retry`]) need no
//! question.

use std::path::Path;

use super::install::{self, InstallDecision};
use super::{Exit, SandboxOptions};
use crate::cli::prompt;
use crate::cli::prompt::choose::{Choice, Choose};
use crate::cli::prompt::style::Tone;
use crate::config::ResolvedConfig;
use crate::oci::{self, ImageChange, ImageChangeStop, OciImage, OnImageChange};
use crate::packs::install::facts;
use crate::packs::install::plan::{self, DecideInput, Pending, Plan, Transition, Wanted, Why};
use crate::packs::install::state::{self as install_state, InstallState, ReadState};
use crate::packs::{InstallerScript, PackManager};
use crate::project::{self, SandboxLock};
use crate::util::PinnedDir;
use crate::vault::Vault;
use crate::{cli, sandbox};

/// The stored sandbox: the lock, the prepared image, and what
/// [`super::install::install_tools`] installs.
pub struct EnsuredSandbox {
    pub lock: SandboxLock,
    pub image: OciImage,
    pub installs: InstallDecision,
}

/// Who answers the sandbox questions.
#[derive(Debug, Clone, Copy)]
struct Answering {
    /// There is a terminal to ask on.
    can_prompt: bool,
    /// `--yes`: the default answer, without a question.
    yes: bool,
}

/// What to do about the tool changes of a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolChanges {
    /// No question: install what is pending (a new disk, retries only, or
    /// nothing changed).
    NoQuestion,
    /// Ask in the terminal.
    Ask,
    /// `--yes`: re-create the sandbox.
    Recreate,
    /// A question is needed, but nobody can answer it (exit code 2).
    NeedsTerminal,
}

/// A tool question: about the changed packs, then about the removed
/// tools, then about the added tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolQuestion {
    Changed,
    Removed,
    Added,
}

/// The answer to a tool question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolAnswer {
    /// Re-create sandbox (all questions).
    Recreate,
    /// Continue with current sandbox (removed tools).
    KeepRemoved,
    /// Install anyways (added tools).
    InstallAdded,
}

/// The gray line before the added-tools question when the sandbox
/// continues with a new image because its old one is gone.
const OLD_IMAGE_GONE_NOTE: &str =
    "The old image is not available any more, so the tools install again for the new image.";

/// What `oci::prepare` does about a changed image: a new disk and `--yes`
/// re-create without a question, a terminal asks, and nobody else can
/// answer.
fn on_image_change(new_disk: bool, answering: Answering) -> OnImageChange {
    if new_disk || answering.yes {
        OnImageChange::Recreate
    } else if answering.can_prompt {
        OnImageChange::Ask
    } else {
        OnImageChange::Refuse
    }
}

/// Whether the disk resets right after the image preparation: a
/// re-created image of an existing disk. `oci::prepare` has already
/// dropped the old image then, so the answer is carried out before any
/// check can stop the run, and no tool question follows.
fn image_resets_disk(change: ImageChange, new_disk: bool) -> bool {
    change == ImageChange::Recreate && !new_disk
}

/// The notes of the added-tools question: a sandbox that continues with a
/// new image because its old one is gone installs its tools again.
fn added_tools_notes(change: ImageChange) -> Vec<&'static str> {
    let mut notes = Vec::new();
    if change == ImageChange::OldImageGone {
        notes.push(OLD_IMAGE_GONE_NOTE);
    }
    notes.extend([
        "The install runs with unrestricted network inside the existing sandbox; code already \
         in the sandbox can run during it.",
        "Re-create the sandbox for a clean install.",
    ]);
    notes
}

/// What to do about the tool changes of `plan`: changed packs, removed
/// tools, and pending tools other than retries. A new disk gets its tools
/// without a question.
fn tool_changes(new_disk: bool, plan: &Plan, answering: Answering) -> ToolChanges {
    let unchanged =
        plan.changed.is_empty() && plan.removed.is_empty() && asked(&plan.pending).is_empty();
    if new_disk || unchanged {
        ToolChanges::NoQuestion
    } else if answering.yes {
        ToolChanges::Recreate
    } else if answering.can_prompt {
        ToolChanges::Ask
    } else {
        ToolChanges::NeedsTerminal
    }
}

/// The pending tools that need the "added" question: all but retries.
fn asked(pending: &[Pending]) -> Vec<&Pending> {
    pending.iter().filter(|p| p.why != Why::Retry).collect()
}

/// The labels of the configured packs `ids` (from their installs).
fn candidate_labels<'a>(
    ids: impl IntoIterator<Item = &'a str>,
    candidates: &[InstallerScript],
) -> Vec<String> {
    ids.into_iter()
        .map(|id| {
            candidates
                .iter()
                .find(|c| c.pack == id)
                .map_or(id, |c| c.label.as_str())
                .to_string()
        })
        .collect()
}

/// The error text for tool changes that nobody can answer.
fn tools_changed_message(plan: &Plan) -> String {
    let added: Vec<&str> = asked(&plan.pending).iter().map(|p| p.id.as_str()).collect();
    let mut parts = Vec::new();
    if !plan.changed.is_empty() {
        parts.push(format!("changed: {}", plan.changed.join(", ")));
    }
    if !added.is_empty() {
        parts.push(format!("added: {}", added.join(", ")));
    }
    if !plan.removed.is_empty() {
        parts.push(format!("removed: {}", plan.removed.join(", ")));
    }
    format!(
        "Tools changed in the sandbox ({}). Run in a terminal or pass --yes.",
        parts.join("; ")
    )
}

/// Store the sandbox for `resolved`: check `[env]`, take the lock, read
/// the install records, prepare the image, answer the image and tool
/// questions, re-create the disk when that is the answer, create (or
/// resize) the disk, and save the decided record changes.
pub async fn ensure_sandbox(
    host_cwd: &Path,
    packs: &PackManager,
    resolved: &ResolvedConfig,
    options: &SandboxOptions,
    vault: &Vault,
) -> Result<EnsuredSandbox, Exit> {
    super::env::check_env_early(&resolved.values, vault)?;
    let lock = SandboxLock::acquire(host_cwd)?;
    let sandbox_dir = PinnedDir::open(host_cwd, Path::new(".airlock/sandbox"), false)
        .map_err(anyhow::Error::from)?;
    let answering = Answering {
        can_prompt: prompt::can_prompt(),
        yes: options.yes,
    };
    let mut state = read_state(&sandbox_dir, answering.can_prompt)?;
    let candidates = install::install_candidates(&resolved.packs);
    let wanted: Vec<Wanted> = candidates
        .iter()
        .map(|t| Wanted {
            id: t.pack.clone(),
            fingerprint: t.fingerprint.clone(),
        })
        .collect();
    let decide = |state: &InstallState, image_id: Option<&str>| {
        plan::decide(&DecideInput {
            state,
            wanted: &wanted,
            disk: project::disk_id(sandbox_dir.path()),
            image_id,
        })
    };
    let new_disk = !project::has_disk(sandbox_dir.path());

    // Stop before the pull when a tool question nobody can answer is needed.
    let early = decide(&state, None);
    if tool_changes(new_disk, &early, answering) == ToolChanges::NeedsTerminal {
        return Err(Exit::error(2, tools_changed_message(&early)));
    }

    let values = &resolved.values;
    sandbox::report::print_preparing(sandbox_dir.path(), &values.vm.image.name);
    let prepared = oci::prepare(
        sandbox_dir.path(),
        &values.vm.image,
        vault,
        on_image_change(new_disk, answering),
    )
    .await
    .map_err(image_error)?;
    let image = prepared.image;

    let image_reset = image_resets_disk(prepared.change, new_disk);
    if image_reset {
        project::reset_disk(&sandbox_dir)?;
    }
    let mut plan = decide(&state, Some(&image.image_id));
    let mut recreate = false;
    if !image_reset {
        match tool_changes(new_disk, &plan, answering) {
            ToolChanges::NoQuestion => {}
            ToolChanges::Recreate => recreate = true,
            ToolChanges::NeedsTerminal => {
                return Err(Exit::error(2, tools_changed_message(&plan)));
            }
            ToolChanges::Ask => {
                recreate = answer_tool_changes(&mut plan, |question, plan| match question {
                    ToolQuestion::Changed => ask_changed_packs(&candidates, plan),
                    ToolQuestion::Removed => ask_removed_tools(packs, plan),
                    ToolQuestion::Added => {
                        ask_added_tools(&candidates, plan, &added_tools_notes(prepared.change))
                    }
                })?;
            }
        }
    }

    // Check the image before the disk of a tool question is gone or
    // anything installs.
    if !candidates.is_empty() && (recreate || !plan.pending.is_empty()) {
        facts::check(&image).map_err(|e| Exit::error(2, e))?;
    }
    if recreate {
        project::reset_disk(&sandbox_dir)?;
        plan = decide(&state, Some(&image.image_id));
    }
    project::ensure_disk(sandbox_dir.path(), &values.disk)?;
    let to_install: Vec<InstallerScript> = candidates
        .into_iter()
        .filter(|t| plan.pending.iter().any(|p| p.id == t.pack))
        .collect();
    let transitions = std::mem::take(&mut plan.transitions);
    let installs = InstallDecision::save(
        sandbox_dir,
        std::mem::take(&mut state),
        &transitions,
        to_install,
        &image,
    )?;
    Ok(EnsuredSandbox {
        lock,
        image,
        installs,
    })
}

/// Ask about the changed packs, then about the removed tools, then about
/// the added tools, of `plan` (each only when there are such packs), with
/// `ask`, and apply the answers (see [`apply_tool_answer`]). A re-create
/// answer ends the questions. Returns whether to re-create the sandbox.
fn answer_tool_changes(
    plan: &mut Plan,
    mut ask: impl FnMut(ToolQuestion, &Plan) -> Result<ToolAnswer, Exit>,
) -> Result<bool, Exit> {
    if !plan.changed.is_empty() {
        let answer = ask(ToolQuestion::Changed, plan)?;
        if apply_tool_answer(plan, answer) {
            return Ok(true);
        }
    }
    if !plan.removed.is_empty() {
        let answer = ask(ToolQuestion::Removed, plan)?;
        if apply_tool_answer(plan, answer) {
            return Ok(true);
        }
    }
    if asked(&plan.pending).is_empty() {
        return Ok(false);
    }
    let answer = ask(ToolQuestion::Added, plan)?;
    Ok(apply_tool_answer(plan, answer))
}

/// Apply a tool answer to `plan`: Continue with current sandbox keeps the
/// removed tools (recorded as `kept`). Returns whether to re-create the
/// sandbox.
fn apply_tool_answer(plan: &mut Plan, answer: ToolAnswer) -> bool {
    match answer {
        ToolAnswer::Recreate => true,
        ToolAnswer::KeepRemoved => {
            let kept = plan.removed.iter().cloned().map(Transition::Keep);
            plan.transitions.extend(kept);
            false
        }
        ToolAnswer::InstallAdded => false,
    }
}

/// Ask about the changed packs of `plan`: only a new sandbox installs
/// them, so the answers are Re-create sandbox and Cancel.
fn ask_changed_packs(candidates: &[InstallerScript], plan: &Plan) -> Result<ToolAnswer, Exit> {
    let labels = candidate_labels(plan.changed.iter().map(String::as_str), candidates);
    ask(
        &format!("Packs have changed: {}", labels.join(", ")),
        &["Changed packs install only into a re-created sandbox."],
        &[("re-create sandbox", ToolAnswer::Recreate)],
    )
}

/// Ask about the removed tools of `plan`.
fn ask_removed_tools(packs: &PackManager, plan: &Plan) -> Result<ToolAnswer, Exit> {
    let labels: Vec<String> = plan.removed.iter().map(|id| label(packs, id)).collect();
    ask(
        &format!(
            "Tools have been removed from sandbox: {}",
            labels.join(", ")
        ),
        &["Removed tools remain in the sandbox until recreation."],
        &[
            ("re-create sandbox", ToolAnswer::Recreate),
            ("continue with current sandbox", ToolAnswer::KeepRemoved),
        ],
    )
}

/// Ask about the added tools of `plan`, with the gray `notes`.
fn ask_added_tools(
    candidates: &[InstallerScript],
    plan: &Plan,
    notes: &[&str],
) -> Result<ToolAnswer, Exit> {
    let labels = candidate_labels(
        asked(&plan.pending).iter().map(|p| p.id.as_str()),
        candidates,
    );
    ask(
        &format!("Tools have been added to sandbox: {}", labels.join(", ")),
        notes,
        &[
            ("re-create sandbox", ToolAnswer::Recreate),
            ("install anyways", ToolAnswer::InstallAdded),
        ],
    )
}

/// Ask `title` with the gray `notes`, the `choices` and a last `cancel`
/// (default: the first). Cancel and Esc end the run.
fn ask(title: &str, notes: &[&str], choices: &[(&str, ToolAnswer)]) -> Result<ToolAnswer, Exit> {
    let options: Vec<Choice> = choices
        .iter()
        .map(|(label, _)| *label)
        .chain(["cancel"])
        .map(|label| Choice {
            label,
            tone: Tone::Plain,
        })
        .collect();
    let question = Choose {
        title,
        notes,
        choices: &options,
        default: 0,
        report: true,
    };
    match question.ask()? {
        Some(i) if i < choices.len() => Ok(choices[i].1),
        _ => Err(Exit::aborted()),
    }
}

/// The run's end for a failed image preparation: a changed image that
/// nobody can answer about (exit code 2), Cancel, Ctrl+C, or a failure.
fn image_error(e: anyhow::Error) -> Exit {
    match e.downcast_ref::<ImageChangeStop>() {
        Some(ImageChangeStop::NeedsTerminal) => Exit::error(2, e),
        Some(ImageChangeStop::Cancelled) => Exit::aborted(),
        Some(ImageChangeStop::Interrupted) => Exit::INTERRUPTED,
        None => Exit::Failed(e),
    }
}

/// The label of pack `id` (the id for a pack airlock does not know).
fn label(packs: &PackManager, id: &str) -> String {
    packs
        .builtin()
        .iter()
        .find(|p| p.metadata().name == id)
        .map_or_else(|| id.to_string(), |p| p.metadata().label.clone())
}

/// Read `installs.json`. A corrupt file counts as empty in a terminal (every
/// pack installs again) and is an error without one.
fn read_state(sandbox: &PinnedDir, can_prompt: bool) -> Result<InstallState, Exit> {
    let path = sandbox.path().join(install_state::STATE_FILE);
    match install_state::read(sandbox) {
        ReadState::Absent => Ok(InstallState::default()),
        ReadState::Ok(state) => Ok(state),
        ReadState::TooNew(version) => {
            cli::error!(
                "{} was written by a newer airlock (format {version}); update airlock, or run \
                 `airlock rm` to start over",
                path.display()
            );
            Err(Exit::Code(2))
        }
        ReadState::Corrupt(why) if can_prompt => {
            cli::log!(
                "{} {} is not valid ({why}); treating every pack as not installed",
                cli::yellow("!"),
                path.display()
            );
            Ok(InstallState::default())
        }
        ReadState::Corrupt(why) => {
            cli::error!(
                "{} is not valid ({why}); run `airlock start` in a terminal, or `airlock rm` \
                 to start over",
                path.display()
            );
            Err(Exit::Code(2))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTY: Answering = Answering {
        can_prompt: true,
        yes: false,
    };
    const YES: Answering = Answering {
        can_prompt: false,
        yes: true,
    };
    const NOBODY: Answering = Answering {
        can_prompt: false,
        yes: false,
    };

    fn pending(id: &str, why: Why) -> Pending {
        Pending { id: id.into(), why }
    }

    fn plan(pending: Vec<Pending>, removed: &[&str]) -> Plan {
        Plan {
            pending,
            removed: removed.iter().map(|r| (*r).to_string()).collect(),
            ..Plan::default()
        }
    }

    /// A plan whose pack `id` changed.
    fn changed_plan(id: &str) -> Plan {
        Plan {
            changed: vec![id.to_string()],
            ..Plan::default()
        }
    }

    #[test]
    fn a_changed_image_asks_in_a_terminal_and_re_creates_with_yes() {
        assert_eq!(on_image_change(false, TTY), OnImageChange::Ask);
        assert_eq!(on_image_change(false, YES), OnImageChange::Recreate);
        assert_eq!(
            on_image_change(
                false,
                Answering {
                    can_prompt: true,
                    yes: true
                }
            ),
            OnImageChange::Recreate
        );
        assert_eq!(on_image_change(false, NOBODY), OnImageChange::Refuse);
    }

    #[test]
    fn a_new_disk_needs_no_question() {
        let changed = plan(vec![pending("python", Why::New)], &["mise"]);
        for answering in [TTY, YES, NOBODY] {
            assert_eq!(on_image_change(true, answering), OnImageChange::Recreate);
            assert_eq!(
                tool_changes(true, &changed, answering),
                ToolChanges::NoQuestion
            );
        }
    }

    #[test]
    fn removed_tools_ask_in_a_terminal_and_re_create_with_yes() {
        let removed = plan(vec![], &["mise"]);
        assert_eq!(tool_changes(false, &removed, TTY), ToolChanges::Ask);
        assert_eq!(tool_changes(false, &removed, YES), ToolChanges::Recreate);
        assert_eq!(
            tool_changes(false, &removed, NOBODY),
            ToolChanges::NeedsTerminal
        );
        assert_eq!(
            tools_changed_message(&removed),
            "Tools changed in the sandbox (removed: mise). Run in a terminal or pass --yes."
        );
    }

    #[test]
    fn added_tools_ask_in_a_terminal_and_re_create_with_yes() {
        for added in [
            plan(vec![pending("python", Why::New)], &[]),
            changed_plan("python"),
        ] {
            assert_eq!(tool_changes(false, &added, TTY), ToolChanges::Ask);
            assert_eq!(tool_changes(false, &added, YES), ToolChanges::Recreate);
            assert_eq!(
                tool_changes(false, &added, NOBODY),
                ToolChanges::NeedsTerminal
            );
        }
        let both = plan(
            vec![pending("python", Why::New), pending("rust", Why::Retry)],
            &["mise"],
        );
        assert_eq!(
            tools_changed_message(&both),
            "Tools changed in the sandbox (added: python; removed: mise). Run in a terminal or \
             pass --yes."
        );
    }

    /// An unfinished install of the same fingerprint installs again
    /// without a question, also without a terminal.
    #[test]
    fn a_retry_needs_no_question() {
        let retry = plan(vec![pending("python", Why::Retry)], &[]);
        for answering in [TTY, YES, NOBODY] {
            assert_eq!(
                tool_changes(false, &retry, answering),
                ToolChanges::NoQuestion
            );
        }
        assert_eq!(
            tool_changes(false, &Plan::default(), NOBODY),
            ToolChanges::NoQuestion
        );
    }

    #[test]
    fn the_questions_name_the_labels() {
        let packs = crate::packs::init().unwrap();
        let resolved = crate::test_support::resolve_project_toml(
            "[packs]\npython = { version = 1 }\nrust = { version = 1 }\n",
        )
        .unwrap();
        let candidates = install::install_candidates(&resolved.packs);
        assert_eq!(
            candidate_labels(["python", "rust", "unknown"], &candidates),
            ["python", "rust", "unknown"]
        );
        assert_eq!(label(&packs, "mise"), "mise");
    }

    /// Continue with current sandbox records the removed tools as kept.
    #[test]
    fn keeping_removed_tools_adds_keep_transitions() {
        let mut changed = plan(vec![pending("python", Why::New)], &["mise", "rust"]);
        assert!(!apply_tool_answer(&mut changed, ToolAnswer::KeepRemoved));
        assert_eq!(
            changed.transitions,
            [
                Transition::Keep("mise".into()),
                Transition::Keep("rust".into())
            ]
        );
        assert_eq!(changed.pending, [pending("python", Why::New)]);
        assert!(!apply_tool_answer(&mut changed, ToolAnswer::InstallAdded));
        assert_eq!(changed.pending, [pending("python", Why::New)]);
        assert!(apply_tool_answer(&mut changed, ToolAnswer::Recreate));
    }

    /// The questions of `changed` with the `answers` in turn: the asked
    /// questions and whether the sandbox is re-created.
    fn questions(changed: &mut Plan, answers: &[ToolAnswer]) -> (Vec<ToolQuestion>, bool) {
        let mut asked_questions = Vec::new();
        let mut answers = answers.iter();
        let recreate = answer_tool_changes(changed, |question, _| {
            asked_questions.push(question);
            Ok(*answers.next().expect("an answer"))
        })
        .unwrap();
        (asked_questions, recreate)
    }

    #[test]
    fn re_creating_at_the_removed_question_skips_the_added_question() {
        let both = || {
            plan(
                vec![pending("python", Why::New), pending("rust", Why::Retry)],
                &["mise"],
            )
        };
        let mut changed = both();
        assert_eq!(
            questions(&mut changed, &[ToolAnswer::Recreate]),
            (vec![ToolQuestion::Removed], true)
        );
        let mut changed = both();
        assert_eq!(
            questions(
                &mut changed,
                &[ToolAnswer::KeepRemoved, ToolAnswer::InstallAdded]
            ),
            (vec![ToolQuestion::Removed, ToolQuestion::Added], false)
        );
        assert_eq!(
            changed.pending,
            [pending("python", Why::New), pending("rust", Why::Retry)]
        );
        assert_eq!(changed.transitions, [Transition::Keep("mise".into())]);
        // Retries only: no added question.
        let mut changed = plan(vec![pending("rust", Why::Retry)], &["mise"]);
        assert_eq!(
            questions(&mut changed, &[ToolAnswer::KeepRemoved]),
            (vec![ToolQuestion::Removed], false)
        );
        let mut changed = Plan {
            removed: vec!["mise".into()],
            ..changed_plan("python")
        };
        assert_eq!(
            questions(&mut changed, &[ToolAnswer::Recreate]),
            (vec![ToolQuestion::Changed], true)
        );
    }

    /// A re-created image resets the disk before the image check (which
    /// may end the run), and asks no tool question; a tool question's
    /// re-create waits for the check.
    #[test]
    fn a_re_created_image_resets_the_disk_before_the_image_check() {
        assert!(image_resets_disk(ImageChange::Recreate, false));
        assert!(!image_resets_disk(ImageChange::Recreate, true));
        for change in [
            ImageChange::Unchanged,
            ImageChange::KeepOld,
            ImageChange::OldImageGone,
        ] {
            assert!(!image_resets_disk(change, false));
        }
    }

    /// A sandbox whose old image is gone continues with the new image: its
    /// records are stale, so every tool installs again, and the added
    /// question says why.
    #[test]
    fn a_gone_old_image_explains_why_the_tools_install_again() {
        let notes = added_tools_notes(ImageChange::OldImageGone);
        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0], OLD_IMAGE_GONE_NOTE);
        for change in [
            ImageChange::Unchanged,
            ImageChange::KeepOld,
            ImageChange::Recreate,
        ] {
            assert!(!added_tools_notes(change).contains(&OLD_IMAGE_GONE_NOTE));
        }

        let state = InstallState {
            disk: Some((1, 2)),
            image_id: Some("sha256:old".into()),
            packs: std::collections::BTreeMap::from([(
                "python".to_string(),
                install_state::Record {
                    status: install_state::PackStatus::Installed,
                    fingerprint: "f".into(),
                    at: 1,
                },
            )]),
            ..InstallState::default()
        };
        let wanted = [Wanted {
            id: "python".into(),
            fingerprint: "f".into(),
        }];
        let new_image = plan::decide(&DecideInput {
            state: &state,
            wanted: &wanted,
            disk: Some((1, 2)),
            image_id: Some("sha256:new"),
        });
        assert_eq!(new_image.pending, [pending("python", Why::New)]);
        assert_eq!(new_image.transitions, [Transition::Reset]);
        assert_eq!(tool_changes(false, &new_image, TTY), ToolChanges::Ask);
    }

    #[test]
    fn a_changed_image_without_an_answer_is_exit_code_2() {
        let e = anyhow::Error::from(ImageChangeStop::NeedsTerminal);
        assert_eq!(
            e.to_string(),
            "Sandbox image has been changed. Run in a terminal or pass --yes."
        );
        assert!(matches!(image_error(e), Exit::Code(2)));
        assert!(matches!(
            image_error(anyhow::anyhow!("pull failed")),
            Exit::Failed(_)
        ));
    }
}
