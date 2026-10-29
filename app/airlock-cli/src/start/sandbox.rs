//! The sandbox step of `airlock start`.
//!
//! Prepares the stored sandbox (image, disk and install records) and decides
//! which tools to install. For an existing sandbox, it can ask the user about
//! image and tool changes. For a changed pack, the question shows what
//! changed since the install. A sandbox from a template also gets the
//! question for changed packs.

#[cfg(not(test))]
use self::choose_changed_terminal as choose_changed;
use super::config_change::{self, CheckInput, Checked};
use super::install::{self, InstallDecision};
use super::{Exit, SandboxOptions};
use crate::cli::prompt::choose::{Choice, Choose, Details};
use crate::cli::prompt::style::{self, Styles};
use crate::config::{LayeredConfig, ResolvedConfig};
use crate::context::Context;
#[cfg(not(test))]
use crate::oci::prepare as prepare_image;
use crate::oci::{ImageChange, ImageChangeStop, OciImage, OnImageChange};
#[cfg(not(test))]
use crate::packs::install::facts::check as check_image;
use crate::packs::install::plan::{self, DecideInput, Pending, Plan, Transition, Wanted, Why};
use crate::packs::install::snapshots::{self as install_snapshots, InstallSnapshots};
use crate::packs::install::state::{self as install_state, InstallState, ReadState};
use crate::packs::snapshot::{self, PackSnapshot};
use crate::packs::{InstallerScript, PackManager};
use crate::project::{self, SandboxLock};
use crate::templates::{self, Template};
#[cfg(test)]
use crate::test_cfg::start::{check_image, choose_changed, prepare_image};
use crate::util::{PinnedDir, VerifiedPath};
use crate::{cli, messages, sandbox};

/// The stored sandbox, ready for the install step.
pub struct EnsuredSandbox {
    /// The sandbox lock. It stays held while this value lives.
    pub lock: SandboxLock,
    /// The prepared container image.
    pub image: OciImage,
    /// The tools that [`super::install::install_tools`] installs.
    pub installs: InstallDecision,
    /// The config of the run. It is the config of the last run if the user
    /// selected it (see [`config_change::check`]).
    pub resolved: ResolvedConfig,
}

/// How the sandbox questions get their answers.
#[derive(Debug, Clone, Copy)]
struct Answering {
    /// A terminal is available for questions.
    can_prompt: bool,
    /// `--yes`: use the default answer, without a question.
    yes: bool,
}

/// The action for the tool changes of a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolChanges {
    /// No question. Install the pending tools (a new disk, retries only, or
    /// no changes).
    NoQuestion,
    /// Ask in the terminal.
    Ask,
    /// `--yes`: re-create the sandbox.
    Recreate,
    /// A question is necessary, but nobody can answer it (exit code 2).
    NeedsTerminal,
}

/// A tool question. The questions come in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolQuestion {
    /// Packs whose definition is different from the one on the disk.
    Changed,
    /// Tools removed from the config.
    Removed,
    /// Tools added to the config.
    Added,
}

/// The answer to a tool question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolAnswer {
    /// Re-create sandbox (all questions).
    Recreate,
    /// Reinstall on top (changed packs).
    Reinstall,
    /// Keep old (changed packs).
    KeepOld,
    /// Continue with current sandbox (removed tools).
    KeepRemoved,
    /// Install anyway (added tools).
    InstallAdded,
}

/// The answer to the changed-packs question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangedAnswer {
    /// Re-create the sandbox: all packs install on a new disk.
    Recreate,
    /// Install the changed packs on top of the disk (see
    /// [`Plan::reinstall_changed`]).
    Reinstall,
    /// Keep the old packs on the disk (see [`Plan::accept_changed`]).
    KeepOld,
}

impl From<ChangedAnswer> for ToolAnswer {
    fn from(answer: ChangedAnswer) -> Self {
        match answer {
            ChangedAnswer::Recreate => Self::Recreate,
            ChangedAnswer::Reinstall => Self::Reinstall,
            ChangedAnswer::KeepOld => Self::KeepOld,
        }
    }
}

/// The changed packs of a sandbox, and what airlock knows about their
/// installs.
pub(crate) struct ChangedPacks<'a> {
    /// Ids of the changed packs (see [`Plan::changed`]).
    pub changed: &'a [String],
    /// The installers of the configured packs.
    pub candidates: &'a [InstallerScript],
    /// The install records of the disk.
    pub state: &'a InstallState,
    /// The install snapshots of the disk.
    pub snapshots: &'a InstallSnapshots,
}

/// Return what `oci::prepare` does about a changed image.
///
/// A new disk and `--yes` re-create without a question. With a terminal,
/// `oci::prepare` asks. Otherwise it refuses, because nobody can answer.
fn on_image_change(new_disk: bool, answering: Answering) -> OnImageChange {
    if new_disk || answering.yes {
        OnImageChange::Recreate
    } else if answering.can_prompt {
        OnImageChange::Ask
    } else {
        OnImageChange::Refuse
    }
}

/// Return true if the disk resets immediately after the image preparation.
/// This is the case for a re-created image of an existing disk.
// `oci::prepare` already removed the old image then. Thus the reset happens
// before a check can stop the run, and no tool question follows.
fn image_resets_disk(change: ImageChange, new_disk: bool) -> bool {
    change == ImageChange::Recreate && !new_disk
}

/// Return the notes of the added-tools question.
///
/// If the sandbox continues with a new image because the old image is gone,
/// the notes tell that the tools install again.
fn added_tools_notes(change: ImageChange) -> Vec<&'static str> {
    let mut notes = Vec::new();
    if change == ImageChange::OldImageGone {
        notes.push(messages::start::OLD_IMAGE_GONE_NOTE);
    }
    notes.extend([
        messages::start::ADDED_TOOLS_NOTE_INTERNET,
        messages::start::ADDED_TOOLS_NOTE_RECREATE,
    ]);
    notes
}

/// Return the action for the tool changes of `plan`.
///
/// Tool changes are changed packs, removed tools, and pending tools other
/// than retries. A new disk gets its tools without a question. A new copy
/// of a template disk gets only the changed-packs question.
/// Args:
///  - `new_disk`: The sandbox had no disk at the start of the run
///  - `copied`: The disk is a new copy of a template disk
///  - `plan`: The install plan
///  - `answering`: How the questions get their answers
fn tool_changes(new_disk: bool, copied: bool, plan: &Plan, answering: Answering) -> ToolChanges {
    let unchanged = if new_disk {
        !copied || plan.changed.is_empty()
    } else {
        plan.changed.is_empty() && plan.removed.is_empty() && asked(&plan.pending).is_empty()
    };
    if unchanged {
        ToolChanges::NoQuestion
    } else if answering.yes {
        ToolChanges::Recreate
    } else if answering.can_prompt {
        ToolChanges::Ask
    } else {
        ToolChanges::NeedsTerminal
    }
}

/// Return the pending tools that need the "added" question: the new
/// tools. Retries and changed packs need no added question.
fn asked(pending: &[Pending]) -> Vec<&Pending> {
    pending.iter().filter(|p| p.why == Why::New).collect()
}

/// Return the labels of the configured packs `ids` (from their installs).
/// The labels are safe for the terminal: a project pack can set any label.
fn candidate_labels<'a>(
    ids: impl IntoIterator<Item = &'a str>,
    candidates: &[InstallerScript],
) -> Vec<String> {
    ids.into_iter()
        .map(|id| {
            let label = candidates
                .iter()
                .find(|c| c.pack == id)
                .map_or(id, |c| c.label.as_str());
            style::escape_controls(label, false).into_owned()
        })
        .collect()
}

/// Return the error text for tool changes that nobody can answer. With
/// `only_changed`, the text names only the changed packs.
fn tools_changed_message(plan: &Plan, only_changed: bool) -> String {
    let added: Vec<&str> = asked(&plan.pending).iter().map(|p| p.id.as_str()).collect();
    let mut parts = Vec::new();
    if !plan.changed.is_empty() {
        parts.push(messages::start::tools_changed_changed(
            plan.changed.join(", "),
        ));
    }
    if only_changed {
        return messages::start::tools_changed(parts.join("; "));
    }
    if !added.is_empty() {
        parts.push(messages::start::tools_changed_added(added.join(", ")));
    }
    if !plan.removed.is_empty() {
        parts.push(messages::start::tools_changed_removed(
            plan.removed.join(", "),
        ));
    }
    messages::start::tools_changed(parts.join("; "))
}

/// Prepare the stored sandbox for `resolved` and decide which tools to
/// install.
///
/// A new sandbox with `template` starts with a copy of the template (see
/// [`templates::seed`]). The copy gets only the changed-packs question. A
/// re-create answer removes the copied disk, and all packs install on a new
/// disk.
///
/// For an existing sandbox, the step can ask these questions:
///  * A changed config (see [`config_change::check`]). The answers are
///    `Abort` (the default), `Accept changes` and `Continue with old
///    configuration`. `--allow-configuration-changes` accepts the changes.
///
/// The default answer of each of these is `Re-create sandbox`:
///  * A changed image
///  * Changed packs (the definition is different from the one on the disk).
///    The question shows the changes since the install. The other answers
///    are `Reinstall on top`, `Keep old` and `Abort`.
///  * Tools removed from the config
///  * Tools added to the config
///
/// `--yes` uses the default without a question. Without a terminal and
/// without `--yes`, a necessary question is an error (exit code 2). A tool
/// question fails before the image pull. These cases need no question: a new
/// disk, an unchanged sandbox, and the retry of an unfinished install with no
/// session after it (see [`Why::Retry`]). After a session, the retry gets the
/// added-tools question.
/// Args:
///  - `context`: Process context. Its vault resolves `[env]` and prepares
///    the image.
///  - `host_cwd`: Canonical project directory on the host
///  - `packs`: Available packs
///  - `config`: Config files of the run
///  - `resolved`: Resolved `config`
///  - `options`: Command-line options
///  - `template`: The template of a new sandbox. An existing sandbox
///    ignores it.
///
/// Returns:
///   The stored sandbox with the config of the run, or the exit for an
///   error or an aborted question.
// Steps: check `[env]`, find or make the sandbox and take its lock (see
// [`super::location::lock_sandbox`]), check the config changes, copy the
// template into a new sandbox, read the install records, prepare the image,
// answer the image and tool questions, re-create the disk if that is the
// answer, create (or resize) the disk, and save the config copy and the
// record changes.
pub async fn ensure_sandbox(
    context: &Context,
    host_cwd: &VerifiedPath,
    packs: &PackManager,
    config: &LayeredConfig,
    resolved: ResolvedConfig,
    options: &SandboxOptions,
    template: Option<&Template>,
) -> Result<EnsuredSandbox, Exit> {
    let vault = &context.vault;
    super::env::check_env_early(&resolved.values, vault)?;
    let answering = Answering {
        can_prompt: options.can_prompt,
        yes: options.yes,
    };
    let lock =
        super::location::lock_sandbox(context, host_cwd, answering.can_prompt, answering.yes)
            .await?;
    let sandbox_dir = lock
        .dir()
        .to_path()
        .and_then(PinnedDir::pin)
        .map_err(anyhow::Error::from)?;
    // Resolve the host paths before the compare. Thus a symlink that points
    // to an other place since the last run is a change.
    let paths = config_change::host_paths(&resolved.values, &context.home, host_cwd);
    // The config question comes before all other sandbox questions.
    let checked = config_change::check(&CheckInput {
        sandbox_dir: lock.dir(),
        resolved: &resolved,
        paths: &paths,
        resolve: crate::config::ResolveInput {
            data_dir: &context.data,
            packs,
            overrides: &options.overrides,
            project_root: Some(host_cwd),
            // The first resolve of the run already asked. This resolve
            // runs under the sandbox lock and must not wait for the user.
            approval: crate::packs::approval::ApprovalMode::Refuse,
        },
        can_prompt: answering.can_prompt,
        yes: answering.yes,
        allow_changes: options.allow_config_changes,
    })
    .await?;
    let (resolved, save_config) = match checked {
        Checked::Current => (resolved, true),
        Checked::Last(last) => {
            super::env::check_env_early(&last.values, vault)?;
            (*last, false)
        }
    };
    // A sandbox from a template starts with a copy of the template disk.
    // It still counts as a new disk: the tools that the template does not
    // have install without a question. But the template can have an older
    // definition of a pack, so changed packs get the question.
    let new_disk = !project::has_disk(lock.dir());
    let mut seeded = SeedGuard(None);
    let mut copied = false;
    if new_disk && let Some(template) = template {
        templates::seed::seed(template, &sandbox_dir, lock.dir())?;
        seeded.0 = Some(lock.dir().clone());
        copied = true;
    }
    let mut state = read_state(&sandbox_dir, answering.can_prompt)?;
    let snapshots = install_snapshots::read(&sandbox_dir);
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
            disk: project::disk_id(&sandbox_dir),
            image_id,
        })
    };

    // Stop before the pull if a tool question is necessary and nobody can
    // answer it.
    // On this exit, the seed guard removes a template copy.
    let early = decide(&state, None);
    if tool_changes(new_disk, copied, &early, answering) == ToolChanges::NeedsTerminal {
        return Err(Exit::error(2, tools_changed_message(&early, new_disk)));
    }

    let values = &resolved.values;
    sandbox::report::print_preparing(lock.dir(), &values.vm.image.name);
    let prepared = prepare_image(
        &context.data,
        lock.dir(),
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
    // The template disk belongs to the template image. With an other
    // image, start with an empty disk.
    if copied && prepared.change != ImageChange::Unchanged {
        project::reset_disk(&sandbox_dir)?;
        state = InstallState::default();
    }
    let mut plan = decide(&state, Some(&image.image_id));
    let mut recreate = false;
    if !image_reset {
        match tool_changes(new_disk, copied, &plan, answering) {
            ToolChanges::NoQuestion => {}
            ToolChanges::Recreate => recreate = true,
            ToolChanges::NeedsTerminal => {
                return Err(Exit::error(2, tools_changed_message(&plan, new_disk)));
            }
            ToolChanges::Ask => {
                let state = &state;
                let snapshots = &snapshots;
                let candidates = &candidates;
                recreate = answer_tool_changes(&mut plan, &wanted, new_disk, |question, plan| {
                    match question {
                        ToolQuestion::Changed => ask_changed_packs(&ChangedPacks {
                            changed: &plan.changed,
                            candidates,
                            state,
                            snapshots,
                        })
                        .map(ToolAnswer::from),
                        ToolQuestion::Removed => ask_removed_tools(packs, plan),
                        ToolQuestion::Added => {
                            ask_added_tools(candidates, plan, &added_tools_notes(prepared.change))
                        }
                    }
                })?;
            }
        }
    }

    // Check the image before a tool question answer removes the disk, or
    // before anything installs.
    if !candidates.is_empty() && (recreate || !plan.pending.is_empty()) {
        check_image(&context.data, &image).map_err(|e| Exit::error(2, e))?;
    }
    if recreate {
        if copied {
            // The new disk does not start from the template.
            templates::seed::drop_disk(&sandbox_dir)?;
            state = InstallState::default();
            copied = false;
        } else {
            project::reset_disk(&sandbox_dir)?;
        }
        plan = decide(&state, Some(&image.image_id));
    }
    project::ensure_disk(&sandbox_dir, &values.disk, copied)?;
    let to_install: Vec<InstallerScript> = candidates
        .into_iter()
        .filter(|t| plan.pending.iter().any(|p| p.id == t.pack))
        .collect();
    // Save the config only now. Thus if a later question ends the run, the
    // next run asks about the config changes again. The config of the last
    // run is not saved, so the next run also asks again.
    if save_config {
        config_change::save(lock.dir(), config, &paths)?;
    }
    let transitions = std::mem::take(&mut plan.transitions);
    let installs = InstallDecision::save(
        sandbox_dir,
        std::mem::take(&mut state),
        &transitions,
        to_install,
        &image,
    )?;
    seeded.keep();
    Ok(EnsuredSandbox {
        lock,
        image,
        installs,
        resolved,
    })
}

/// The sandbox directory of a template copy, until the sandbox step
/// succeeds. On drop, it removes the copy (see [`templates::seed::unseed`]).
/// Thus after a failed first start, the next start sees a new sandbox, and
/// the setup wizard runs again.
struct SeedGuard(Option<VerifiedPath>);

impl SeedGuard {
    /// Keep the copy: the sandbox is ready.
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for SeedGuard {
    fn drop(&mut self) {
        if let Some(dir) = &self.0 {
            templates::seed::unseed(dir);
        }
    }
}

/// Ask the tool questions for `plan` and apply the answers (see
/// [`apply_tool_answer`]).
///
/// The order is: changed packs, removed tools, added tools. Each question
/// comes only if there are such packs. A re-create answer ends the questions.
/// Args:
///  - `plan`: Install plan. The answers change it.
///  - `wanted`: The configured packs of `plan`
///  - `only_changed`: Ask only the changed-packs question (a new copy of a
///    template disk)
///  - `ask`: Asks one question and returns the answer
///
/// Returns:
///   True if the sandbox must be re-created.
fn answer_tool_changes(
    plan: &mut Plan,
    wanted: &[Wanted],
    only_changed: bool,
    mut ask: impl FnMut(ToolQuestion, &Plan) -> Result<ToolAnswer, Exit>,
) -> Result<bool, Exit> {
    if !plan.changed.is_empty() {
        let answer = ask(ToolQuestion::Changed, plan)?;
        if apply_tool_answer(plan, wanted, answer) {
            return Ok(true);
        }
    }
    if only_changed {
        return Ok(false);
    }
    if !plan.removed.is_empty() {
        let answer = ask(ToolQuestion::Removed, plan)?;
        if apply_tool_answer(plan, wanted, answer) {
            return Ok(true);
        }
    }
    if asked(&plan.pending).is_empty() {
        return Ok(false);
    }
    let answer = ask(ToolQuestion::Added, plan)?;
    Ok(apply_tool_answer(plan, wanted, answer))
}

/// Apply a tool answer to `plan`.
///
/// "Reinstall on top" makes the changed packs pending, "Keep old" records
/// their new fingerprints (see [`Plan`]). "Continue with current sandbox"
/// keeps the removed tools (recorded as `kept`).
/// Args:
///  - `wanted`: The configured packs of `plan`
///
/// Returns:
///   True if the sandbox must be re-created.
fn apply_tool_answer(plan: &mut Plan, wanted: &[Wanted], answer: ToolAnswer) -> bool {
    match answer {
        ToolAnswer::Recreate => true,
        ToolAnswer::Reinstall => {
            plan.reinstall_changed(wanted);
            false
        }
        ToolAnswer::KeepOld => {
            plan.accept_changed(wanted);
            false
        }
        ToolAnswer::KeepRemoved => {
            let kept = plan.removed.iter().cloned().map(Transition::Keep);
            plan.transitions.extend(kept);
            false
        }
        ToolAnswer::InstallAdded => false,
    }
}

/// Get the answer to the changed-packs question for `packs`, outside of
/// [`ensure_sandbox`]. `--yes` re-creates without a question.
/// Args:
///  - `options`: Command-line options
///  - `packs`: The changed packs
///
/// Returns:
///   The answer, or the exit if nobody can answer (exit code 2) or the
///   user aborts.
pub(crate) fn changed_packs_answer(
    options: &super::SandboxOptions,
    packs: &ChangedPacks<'_>,
) -> Result<ChangedAnswer, Exit> {
    if options.yes {
        Ok(ChangedAnswer::Recreate)
    } else if options.can_prompt {
        ask_changed_packs(packs)
    } else {
        let parts = messages::start::tools_changed_changed(packs.changed.join(", "));
        Err(Exit::error(2, messages::start::tools_changed(parts)))
    }
}

/// Ask about the changed packs `packs`. A key prints the changes since the
/// install of each pack above the question.
fn ask_changed_packs(packs: &ChangedPacks<'_>) -> Result<ChangedAnswer, Exit> {
    let styles = Styles::new();
    let mut items = Vec::new();
    let mut lines = Vec::new();
    for id in packs.changed {
        let Some(candidate) = packs.candidates.iter().find(|c| &c.pack == id) else {
            let id = style::escape_controls(id, false);
            items.push(messages::packs::pack_item(&id, &id));
            continue;
        };
        items.push(messages::packs::pack_item(
            style::escape_controls(&candidate.pack, false),
            style::escape_controls(&candidate.label, false),
        ));
        let installed = packs
            .state
            .packs
            .get(id)
            .and_then(|record| packs.snapshots.get(&record.fingerprint));
        lines.extend(changed_pack_lines(candidate, installed, &styles));
    }
    choose_changed(&items, &lines)
}

/// Return the lines that show the changes of the pack of `candidate` since
/// its install: the file diff, the version and arg diff, and the names of
/// the changed setup values. All text from the pack is made safe for the
/// terminal.
/// Args:
///  - `candidate`: Installer of the current pack definition
///  - `installed`: Snapshot of the install, if the sandbox has it
///  - `styles`: Colors of the lines
fn changed_pack_lines(
    candidate: &InstallerScript,
    installed: Option<&PackSnapshot>,
    styles: &Styles,
) -> Vec<String> {
    let new = &candidate.snapshot;
    let id = style::escape_controls(&candidate.pack, false);
    let label = style::escape_controls(&candidate.label, false);
    let mut lines = vec![messages::start::changed_pack_header(id, label)];
    let Some(installed) = installed else {
        lines.push(format!("  {}", messages::packs::CHANGES_NOT_SHOWN));
        return lines;
    };
    lines.extend(snapshot::file_lines(installed, new, styles));
    lines.extend(snapshot::args_lines(installed, new, styles));
    let keys = snapshot::changed_env_keys(installed, new);
    if !keys.is_empty() {
        let keys: Vec<String> = keys
            .iter()
            .map(|key| style::escape_controls(key, false).into_owned())
            .collect();
        lines.push(messages::start::setup_values_changed(keys.join(", ")));
    }
    lines
}

/// Ask the changed-packs question. The default answer is `Re-create
/// sandbox`.
/// Args:
///  - `items`: The list of the changed packs
///  - `lines`: The changes, which a key shows and hides in the question
///
/// Returns:
///   The answer. Abort and Esc end the run.
pub(crate) fn choose_changed_terminal(
    items: &[String],
    lines: &[String],
) -> Result<ChangedAnswer, Exit> {
    let notes: Vec<&str> = items.iter().map(String::as_str).collect();
    let details = Details {
        key: 's',
        show_hint: messages::packs::SHOW_CHANGES_HINT,
        hide_hint: messages::packs::HIDE_CHANGES_HINT,
        lines,
    };
    let answer = ask_with(
        messages::start::PACKS_CHANGED,
        &notes,
        &[
            (messages::start::RECREATE_SANDBOX, ToolAnswer::Recreate),
            (messages::start::REINSTALL_ON_TOP, ToolAnswer::Reinstall),
            (messages::start::KEEP_OLD_PACKS, ToolAnswer::KeepOld),
        ],
        Some(&details),
    )?;
    Ok(match answer {
        ToolAnswer::Reinstall => ChangedAnswer::Reinstall,
        ToolAnswer::KeepOld => ChangedAnswer::KeepOld,
        _ => ChangedAnswer::Recreate,
    })
}

/// Ask about the removed tools of `plan`.
fn ask_removed_tools(packs: &PackManager, plan: &Plan) -> Result<ToolAnswer, Exit> {
    let labels: Vec<String> = plan.removed.iter().map(|id| label(packs, id)).collect();
    ask(
        &messages::start::tools_removed(labels.join(", ")),
        &[messages::start::TOOLS_REMOVED_NOTE],
        &[
            (messages::start::RECREATE_SANDBOX, ToolAnswer::Recreate),
            (messages::start::KEEP_SANDBOX, ToolAnswer::KeepRemoved),
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
        &messages::start::tools_added(labels.join(", ")),
        notes,
        &[
            (messages::start::RECREATE_SANDBOX, ToolAnswer::Recreate),
            (messages::start::INSTALL_ANYWAYS, ToolAnswer::InstallAdded),
        ],
    )
}

/// Ask a question with a last `Abort` choice.
/// Args:
///  - `title`: The question
///  - `notes`: Gray lines below the question
///  - `choices`: Labels and answers. The first is the default.
///
/// Returns:
///   The selected answer. Abort and Esc end the run.
fn ask(title: &str, notes: &[&str], choices: &[(&str, ToolAnswer)]) -> Result<ToolAnswer, Exit> {
    ask_with(title, notes, choices, None)
}

/// [`ask`] with the optional `details` key (see [`Choose::ask_with_details`]).
fn ask_with(
    title: &str,
    notes: &[&str],
    choices: &[(&str, ToolAnswer)],
    details: Option<&Details>,
) -> Result<ToolAnswer, Exit> {
    let options: Vec<Choice> = choices
        .iter()
        .map(|(label, _)| *label)
        .chain([messages::cli::ABORT_CHOICE])
        .map(|label| Choice { label })
        .collect();
    let question = Choose {
        title,
        notes,
        choices: &options,
        default: 0,
        report: true,
    };
    let answer = match details {
        Some(details) => question.ask_with_details(details)?,
        None => question.ask()?,
    };
    match answer {
        Some(i) if i < choices.len() => Ok(choices[i].1),
        _ => Err(Exit::aborted()),
    }
}

/// Convert a failed image preparation to the end of the run.
///
/// The cases: a changed image that nobody can answer about (exit code 2),
/// Abort, Ctrl+C, or a failure.
fn image_error(e: anyhow::Error) -> Exit {
    match e.downcast_ref::<ImageChangeStop>() {
        Some(ImageChangeStop::NeedsTerminal | ImageChangeStop::ImportedImageUnknown) => {
            Exit::error(2, e)
        }
        Some(ImageChangeStop::Aborted) => Exit::aborted(),
        Some(ImageChangeStop::Interrupted) => Exit::INTERRUPTED,
        None => Exit::Failed(e),
    }
}

/// Return the label of pack `id`, or `id` for a pack that airlock does not
/// know. The label is safe for the terminal.
fn label(packs: &PackManager, id: &str) -> String {
    let offered = packs.offered();
    let label = offered
        .iter()
        .find(|p| p.metadata().id == id)
        .map_or(id, |p| p.metadata().label.as_str());
    style::escape_controls(label, false).into_owned()
}

/// Read `installs.json`.
///
/// With a terminal, a corrupt file counts as empty (every pack installs
/// again). Without a terminal, a corrupt file is an error.
fn read_state(sandbox: &PinnedDir, can_prompt: bool) -> Result<InstallState, Exit> {
    let path = sandbox.path().join(install_state::STATE_FILE);
    match install_state::read(sandbox) {
        ReadState::Absent => Ok(InstallState::default()),
        ReadState::Ok(state) => Ok(state),
        ReadState::TooNew(version) => {
            cli::error!(
                "{}",
                messages::start::state_too_new(path.display(), version)
            );
            Err(Exit::Code(2))
        }
        ReadState::Corrupt(why) if can_prompt => {
            cli::log!(
                "{} {}",
                cli::yellow("!"),
                messages::start::state_corrupt_ignored(path.display(), why)
            );
            Ok(InstallState::default())
        }
        ReadState::Corrupt(why) => {
            cli::error!("{}", messages::start::state_corrupt(path.display(), why));
            Err(Exit::Code(2))
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the sandbox step decisions: when to ask about image and
    //! tool changes, the order of the tool questions, and their texts.

    use super::*;
    use crate::test_cfg::packs::{test_installers, test_packs};

    /// A terminal without `--yes`.
    const TTY: Answering = Answering {
        can_prompt: true,
        yes: false,
    };

    /// A pending pack `id` with reason `why`.
    fn pending(id: &str, why: Why) -> Pending {
        Pending { id: id.into(), why }
    }

    /// A plan with `pending`, `removed` and `changed` packs and no
    /// transitions.
    fn plan(pending: Vec<Pending>, removed: &[&str], changed: &[&str]) -> Plan {
        Plan {
            pending,
            removed: removed.iter().map(ToString::to_string).collect(),
            changed: changed.iter().map(ToString::to_string).collect(),
            ..Plan::default()
        }
    }

    /// Ask the tool questions for `plan` with `answers` in order.
    /// Returns:
    ///   The questions asked, and true if the sandbox must be re-created.
    fn questions(plan: &mut Plan, answers: &[ToolAnswer]) -> (Vec<ToolQuestion>, bool) {
        let mut asked_questions = Vec::new();
        let mut answers = answers.iter();
        let recreate = answer_tool_changes(plan, &[], false, |question, _| {
            asked_questions.push(question);
            Ok(*answers.next().expect("an answer"))
        })
        .unwrap();
        (asked_questions, recreate)
    }

    /// Test that a terminal asks about image and tool changes, and that
    /// retries alone ask nothing. A new disk or `--yes` gives no question.
    ///   1. Check the image change action for a terminal, a new disk and
    ///      `--yes`
    ///   2. For an added, removed and changed pack, check the tool change
    ///      action for a terminal, `--yes` and a new disk
    ///   3. Check that a plan with only a retry asks nothing
    #[test]
    fn terminal_asks_about_image_and_tool_changes_but_not_about_retries() {
        assert_eq!(on_image_change(false, TTY), OnImageChange::Ask);
        assert_eq!(on_image_change(true, TTY), OnImageChange::Recreate);
        let yes = Answering {
            can_prompt: true,
            yes: true,
        };
        assert_eq!(on_image_change(false, yes), OnImageChange::Recreate);
        for changed in [
            plan(vec![pending("alpha", Why::New)], &[], &[]),
            plan(vec![], &["beta"], &[]),
            plan(vec![], &[], &["gamma"]),
        ] {
            assert_eq!(tool_changes(false, false, &changed, TTY), ToolChanges::Ask);
            assert_eq!(
                tool_changes(false, false, &changed, yes),
                ToolChanges::Recreate
            );
            assert_eq!(
                tool_changes(true, false, &changed, TTY),
                ToolChanges::NoQuestion
            );
        }
        let retry = plan(vec![pending("alpha", Why::Retry)], &[], &[]);
        assert_eq!(
            tool_changes(false, false, &retry, TTY),
            ToolChanges::NoQuestion
        );
    }

    /// Test that the tool questions come in the order changed, removed,
    /// added, and that a re-create answer ends them.
    ///   1. Answer re-create to the changed question and check that no
    ///      other question follows
    ///   2. With no changed packs, answer re-create to the removed question
    ///      and check that it is the only question
    ///   3. Keep the removed tools and install the added ones, and check
    ///      that the removed tools get keep transitions
    ///   4. Check that retries alone do not ask the added question
    #[test]
    fn tool_questions_come_in_order_and_re_create_answer_ends_them() {
        let all = || {
            plan(
                vec![pending("alpha", Why::New), pending("beta", Why::Retry)],
                &["gamma"],
                &["plain"],
            )
        };
        assert_eq!(
            questions(&mut all(), &[ToolAnswer::Recreate]),
            (vec![ToolQuestion::Changed], true)
        );

        let mut kept = plan(
            vec![pending("alpha", Why::New), pending("beta", Why::Retry)],
            &["gamma", "plain"],
            &[],
        );
        assert_eq!(
            questions(&mut kept, &[ToolAnswer::Recreate]),
            (vec![ToolQuestion::Removed], true)
        );
        assert_eq!(
            questions(
                &mut kept,
                &[ToolAnswer::KeepRemoved, ToolAnswer::InstallAdded]
            ),
            (vec![ToolQuestion::Removed, ToolQuestion::Added], false)
        );
        assert_eq!(
            kept.transitions,
            [
                Transition::Keep("gamma".into()),
                Transition::Keep("plain".into())
            ]
        );
        assert_eq!(
            kept.pending,
            [pending("alpha", Why::New), pending("beta", Why::Retry)]
        );

        let mut retries_only = plan(vec![pending("beta", Why::Retry)], &["gamma"], &[]);
        assert_eq!(
            questions(&mut retries_only, &[ToolAnswer::KeepRemoved]),
            (vec![ToolQuestion::Removed], false)
        );
    }

    /// Test that only a re-created image of an existing disk resets the
    /// disk, and that only a gone old image gives the extra note.
    ///   1. Check that the reset happens only for a re-created image and an
    ///      existing disk
    ///   2. Check that the added-tools notes start with the gone-image note
    ///      only when the old image is gone
    #[test]
    fn re_created_image_resets_existing_disk_and_gone_old_image_is_explained() {
        assert!(image_resets_disk(ImageChange::Recreate, false));
        assert!(!image_resets_disk(ImageChange::Recreate, true));
        for change in [
            ImageChange::Unchanged,
            ImageChange::KeepOld,
            ImageChange::OldImageGone,
        ] {
            assert!(!image_resets_disk(change, false));
        }
        assert_eq!(
            added_tools_notes(ImageChange::OldImageGone)[0],
            "The old image is not available any more, so the tools install again for the new image."
        );
        for change in [
            ImageChange::Unchanged,
            ImageChange::KeepOld,
            ImageChange::Recreate,
        ] {
            assert!(!added_tools_notes(change).contains(&"The old image is not available any more, so the tools install again for the new image."));
        }
    }

    /// Test that the tool change texts name the packs, but not retries, and
    /// use the pack label where airlock knows the pack.
    ///   1. Check the error text for a plan with changed, added, retried and
    ///      removed packs
    ///   2. Check the labels of a known and an unknown configured pack
    ///   3. Check the labels of a known and an unknown pack name
    #[test]
    fn tool_change_texts_name_packs_without_retries() {
        let changed = plan(
            vec![pending("alpha", Why::New), pending("beta", Why::Retry)],
            &["gamma"],
            &["plain"],
        );
        assert_eq!(
            tools_changed_message(&changed, false),
            "Tools changed in the sandbox (changed: plain; added: alpha; removed: gamma). Run in \
             a terminal or pass --yes."
        );
        let installers = test_installers("[packs]\nalpha = { version = 1 }\n");
        assert_eq!(
            candidate_labels(["alpha", "unknown"], &installers),
            ["Alpha", "unknown"]
        );
        assert_eq!(label(&test_packs(), "beta"), "Beta");
        assert_eq!(label(&test_packs(), "unknown"), "unknown");
    }

    /// Test that a new copy of a template disk asks only about changed
    /// packs. The template can have an older pack, but added and removed
    /// tools need no question on a new disk.
    ///   1. Check the action for a copy with a changed pack, and with only
    ///      added and removed tools
    ///   2. Answer the questions for the copy and check that only the
    ///      changed question comes
    ///   3. Check that the error text names only the changed pack
    #[test]
    fn template_copy_asks_only_about_changed_packs() {
        let all = || plan(vec![pending("alpha", Why::New)], &["gamma"], &["plain"]);
        assert_eq!(tool_changes(true, true, &all(), TTY), ToolChanges::Ask);
        let no_changed = plan(vec![pending("alpha", Why::New)], &["gamma"], &[]);
        assert_eq!(
            tool_changes(true, true, &no_changed, TTY),
            ToolChanges::NoQuestion
        );
        let mut copy = all();
        let mut asked_questions = Vec::new();
        let recreate = answer_tool_changes(&mut copy, &[], true, |question, _| {
            asked_questions.push(question);
            Ok(ToolAnswer::Reinstall)
        })
        .unwrap();
        assert!(!recreate);
        assert_eq!(asked_questions, [ToolQuestion::Changed]);
        assert_eq!(
            tools_changed_message(&all(), true),
            "Tools changed in the sandbox (changed: plain). Run in a terminal or pass --yes."
        );
    }

    /// Test that the changed-pack lines show the file diff since the
    /// install, and tell when the sandbox has no copy of the install.
    ///   1. Make an installer with a changed file and its old snapshot
    ///   2. Check that the lines show the file diff
    ///   3. Check the lines with no install snapshot
    #[test]
    fn changed_pack_lines_show_file_diff_or_missing_snapshot() {
        let mut candidate = test_installers("[packs]\nalpha = { version = 1 }\n").remove(0);
        let installed = PackSnapshot::new("alpha", [("setup.sh", b"echo a\n".as_slice())]);
        candidate.snapshot = PackSnapshot::new("alpha", [("setup.sh", b"echo b\n".as_slice())]);
        let lines = |installed: Option<&PackSnapshot>| -> Vec<String> {
            changed_pack_lines(&candidate, installed, &Styles::new())
                .iter()
                .map(|line| console::strip_ansi_codes(line).into_owned())
                .collect()
        };
        assert_eq!(
            lines(Some(&installed)),
            [
                "Changes of pack alpha (Alpha) since the install:",
                "setup.sh",
                "- echo a",
                "+ echo b"
            ]
        );
        assert_eq!(
            lines(None),
            [
                "Changes of pack alpha (Alpha) since the install:",
                format!("  {}", messages::packs::CHANGES_NOT_SHOWN).as_str()
            ]
        );
    }
}
