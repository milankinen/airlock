//! Pack approval.
//!
//! A user pack or a project pack can have a config script that runs on the
//! host. The guest can change the project files, and a pull can change a
//! pack. Thus the user must approve a new or changed config script before
//! it runs. Built-in packs and packs with no config script need no
//! approval. Other pack files do not run on the host, thus a change of
//! them needs no approval.
//!
//! An approval is for the exact script text, as airlock read it. The
//! script that runs is always the approved text in memory. Airlock does not
//! read the files again after the approval.
//!
//! The approvals stay in the data directory, with a copy of the approved
//! script. Thus the question for a changed pack can show the changes since
//! the last approval. The question for a new pack can show all of the
//! script. A pack that the user selects in the setup wizard counts as
//! approved.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::prompt;
use crate::cli::prompt::choose::{Choice, Choose, Details};
use crate::cli::prompt::style::{self, Styles};
use crate::data_dir::DataDir;
use crate::packs::{ConfiguredPack, Pack, PackId, PackSource};
use crate::util::VerifiedPath;
use crate::{cli, messages};

/// Name of the approval file in the data directory.
pub const STORE_FILE: &str = "pack-approvals.json";
/// Maximum size of the approval file in bytes.
// One record holds at most the file size limit of 256 KiB as text. JSON
// escapes make that at most 1.5 MiB, so one record always fits. The write
// removes the oldest records until the file fits, so the read never finds
// a file of airlock that is too large.
const STORE_CAP: u64 = 32 * 1024 * 1024;
/// Maximum number of approvals in the file. The oldest approvals go first.
const MAX_RECORDS: usize = 128;
/// Format version of the approval file.
// Version 1 kept a copy of all pack files. An approval file of an other
// version has no approvals, thus all packs need approval again.
const STORE_VERSION: u32 = 2;

/// How the config resolve gets the approval of a new or changed pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Ask the user in the terminal.
    Prompt,
    /// Do not ask. A pack without approval is an error
    /// ([`PacksNeedApproval`]).
    Refuse,
    /// Do not ask. Approve each pack that needs approval and show the pack
    /// ids (`--auto-approve-packs`).
    Auto,
    /// Do not ask, and use the answer as if the user gave it.
    #[cfg(test)]
    Answer(Decision),
}

#[cfg(test)]
impl ApprovalMode {
    /// Approve each pack as if the user answered the question, for tests.
    pub const APPROVE: Self = Self::Answer(Decision::Approve);
}

/// The answer to the approval question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Approve the pack. The approval stays for the next runs.
    Approve,
    /// Stop the run.
    Abort,
}

/// Error: packs need approval, and the mode is [`ApprovalMode::Refuse`].
#[derive(Debug, thiserror::Error)]
#[error("{}", messages::packs::packs_need_approval(
    .0.iter().map(PackId::as_str).collect::<Vec<_>>().join(", ")
))]
pub struct PacksNeedApproval(pub Vec<PackId>);

/// Error: the user aborted the approval question.
#[derive(Debug, thiserror::Error)]
#[error("{}", messages::packs::APPROVAL_ABORTED)]
pub struct ApprovalAborted;

/// Proof that a pack can run its config script: the pack is built in, has
/// no config script, or the user approved its config script. Only this
/// module makes it. Thus no code can run a config script without the
/// approval check (see [`ConfiguredPack::config_values`]).
#[derive(Clone)]
pub struct Approved {
    /// The approved pack, with its config script in memory.
    pack: Pack,
}

impl Approved {
    /// Return true if the token is for `pack`: the same files in memory.
    pub(super) fn covers(&self, pack: &Pack) -> bool {
        self.pack.same_as(pack)
    }

    /// Make a token for `pack` with no check, for tests.
    #[cfg(test)]
    pub fn for_test(pack: &Pack) -> Self {
        Self { pack: pack.clone() }
    }
}

/// Check that the user approved the config script of each user pack and
/// project pack of `packs`. In the mode [`ApprovalMode::Auto`], approve
/// all without a question. In the mode [`ApprovalMode::Prompt`], ask the
/// user one question about all packs that have no approval for their
/// current config script.
/// Args:
///  - `data_dir`: Airlock data directory with the approval file
///  - `packs`: Configured packs
///  - `mode`: How to get a missing approval
///
/// Returns:
///   One token for each pack, in the same order. An error
///   [`PacksNeedApproval`] in the mode [`ApprovalMode::Refuse`],
///   [`ApprovalAborted`] if the user aborts, a
///   [`prompt::PromptError`] if the question fails, or an error if the
///   approval file cannot be read or written.
pub fn approve(
    data_dir: &DataDir,
    packs: &[ConfiguredPack],
    mode: ApprovalMode,
) -> anyhow::Result<Vec<Approved>> {
    match mode {
        ApprovalMode::Refuse => approve_with(data_dir, packs, None),
        ApprovalMode::Prompt => approve_with(data_dir, packs, Some(&mut ask_terminal)),
        ApprovalMode::Auto => approve_with(data_dir, packs, Some(&mut auto_approve)),
        #[cfg(test)]
        ApprovalMode::Answer(decision) => {
            approve_with(data_dir, packs, Some(&mut |_: &[Pending<'_>]| Ok(decision)))
        }
    }
}

/// Approve all packs without a question. Show one line with the pack ids.
#[allow(clippy::unnecessary_wraps)] // same type as the question
fn auto_approve(pending: &[Pending<'_>]) -> anyhow::Result<Decision> {
    let ids: Vec<_> = pending.iter().map(|p| p.name().0).collect();
    cli::log!(
        "{} {}",
        cli::yellow("!"),
        messages::packs::packs_auto_approved(ids.join(", "))
    );
    Ok(Decision::Approve)
}

/// The approval question for all packs that need approval.
type Ask<'a> = dyn FnMut(&[Pending<'_>]) -> anyhow::Result<Decision> + 'a;

/// [`approve`] with the question `ask`.
/// Args:
///  - `ask`: The question. `None`: do not ask, and a pack without approval
///    is an error.
fn approve_with(
    data_dir: &DataDir,
    packs: &[ConfiguredPack],
    ask: Option<&mut Ask<'_>>,
) -> anyhow::Result<Vec<Approved>> {
    let mut checked = Vec::new();
    for configured in packs {
        if let Some(identity) = identity(configured.pack())? {
            checked.push((configured.pack(), identity));
        }
    }
    if checked.is_empty() {
        return Ok(tokens(packs));
    }
    let file = store_file(data_dir)?;
    let mut store = Store::read(&file)?;
    // A pack can show up two times. Then it shows one time.
    let mut keys = BTreeSet::new();
    let pending: Vec<_> = checked
        .into_iter()
        .filter(|(_, identity)| !store.approves(identity) && keys.insert(identity.key.clone()))
        .collect();
    if pending.is_empty() {
        return Ok(tokens(packs));
    }
    let Some(ask) = ask else {
        let ids = pending
            .iter()
            .map(|(pack, _)| pack.metadata().id.clone())
            .collect();
        return Err(PacksNeedApproval(ids).into());
    };
    let questions: Vec<Pending> = pending
        .iter()
        .map(|(pack, identity)| Pending {
            pack,
            last: store
                .packs
                .get(&identity.key)
                .map(|record| record.config_lua.as_str()),
        })
        .collect();
    match ask(&questions)? {
        Decision::Approve => {}
        Decision::Abort => return Err(ApprovalAborted.into()),
    }
    for (pack, identity) in pending {
        store.insert(pack, identity);
    }
    store.write(&file)?;
    Ok(tokens(packs))
}

/// Record the approval of a pack that the user selected in the setup
/// wizard. The selection counts as approval. Only the setup wizard calls
/// this, when the user confirms the answers.
/// Args:
///  - `data_dir`: Airlock data directory with the approval file
///  - `pack`: The selected pack
///
/// Returns:
///   The token for `pack`, or an error if the approval file cannot be read
///   or written.
pub(crate) fn approve_selected(data_dir: &DataDir, pack: &Pack) -> anyhow::Result<Approved> {
    if let Some(identity) = identity(pack)? {
        let file = store_file(data_dir)?;
        let mut store = Store::read(&file)?;
        if !store.approves(&identity) {
            store.insert(pack, identity);
            store.write(&file)?;
        }
    }
    Ok(Approved { pack: pack.clone() })
}

/// Get the token for `pack` if it needs no question: the pack is built
/// in, has no config script, or the approval file has an approval for its
/// config script. Records nothing.
/// Args:
///  - `data_dir`: Airlock data directory with the approval file
///  - `pack`: The pack
///
/// Returns:
///   The token, `None` if the pack needs approval, or an error if the
///   approval file cannot be read.
pub(crate) fn approved(data_dir: &DataDir, pack: &Pack) -> anyhow::Result<Option<Approved>> {
    if let Some(identity) = identity(pack)?
        && !Store::read(&store_file(data_dir)?)?.approves(&identity)
    {
        return Ok(None);
    }
    Ok(Some(Approved { pack: pack.clone() }))
}

/// The tokens for `packs`, after the check.
fn tokens(packs: &[ConfiguredPack]) -> Vec<Approved> {
    packs
        .iter()
        .map(|configured| Approved {
            pack: configured.pack().clone(),
        })
        .collect()
}

/// Where the approval of a pack goes, and what it covers.
struct Identity {
    /// Key in the approval file: the kind and the resolved path of the pack
    /// directory. For a project pack, this is the resolved project root and the
    /// pack path. Thus the same pack path in two projects has two
    /// approvals.
    key: String,
    /// SHA-256 of the config script in memory, as hex.
    hash: String,
}

/// Get the identity of `pack`.
/// Returns:
///   The identity, `None` for a built-in pack or a pack with no config
///   script, or an error if the pack directory cannot be found again.
fn identity(pack: &Pack) -> anyhow::Result<Option<Identity>> {
    let metadata = pack.metadata();
    let (kind, dir) = match &metadata.source {
        PackSource::Builtin => return Ok(None),
        PackSource::Local { dir } => ("local", dir),
        PackSource::Project { dir, .. } => ("project", dir),
    };
    // Only the config script runs on the host. A static config only sets
    // values, and the other files run only in the sandbox.
    let Some(lua) = pack.lua_config() else {
        return Ok(None);
    };
    let path = dir
        .to_entry_path()
        .with_context(|| messages::packs::approval_key_failed(&metadata.id))?;
    // The hash is of the same text in memory that runs later.
    Ok(Some(Identity {
        key: format!("{kind}:{}", path.display()),
        hash: hex::encode(Sha256::digest(lua.as_bytes())),
    }))
}

/// The approval file.
#[derive(Default, Serialize, Deserialize)]
struct Store {
    /// Format version (see [`STORE_VERSION`]).
    version: u32,
    /// The approvals by key (see [`Identity::key`]).
    packs: BTreeMap<String, Record>,
}

/// One approval.
#[derive(Serialize, Deserialize)]
struct Record {
    /// SHA-256 of the approved config script, as hex.
    config_hash: String,
    /// Time of the approval, in seconds since the Unix epoch.
    approved_at: u64,
    /// Copy of the approved config script.
    config_lua: String,
}

/// Get the approval file of `data_dir`.
fn store_file(data_dir: &DataDir) -> anyhow::Result<VerifiedPath> {
    Ok(data_dir.root.join(STORE_FILE)?)
}

impl Store {
    /// Read the approval file `file`.
    /// Returns:
    ///   The approvals. No approvals if the file does not exist, is too
    ///   large or is not valid: then all packs need approval again. An error
    ///   if the read fails.
    fn read(file: &VerifiedPath) -> anyhow::Result<Self> {
        let text = match file.read_to_string_capped(STORE_CAP) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                tracing::warn!("pack approvals: {file}: {e}");
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(
                    anyhow::Error::from(e).context(messages::packs::approvals_file_failed(file))
                );
            }
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(store) if store.version == STORE_VERSION => Ok(store),
            Ok(store) => {
                tracing::warn!("pack approvals: {file}: version {}", store.version);
                Ok(Self::default())
            }
            Err(e) => {
                tracing::warn!("pack approvals: {file}: {e}");
                Ok(Self::default())
            }
        }
    }

    /// Return true if the file has an approval for the config script of
    /// `identity`.
    fn approves(&self, identity: &Identity) -> bool {
        self.packs
            .get(&identity.key)
            .is_some_and(|record| record.config_hash == identity.hash)
    }

    /// Add the approval of `pack`. It replaces an older approval.
    fn insert(&mut self, pack: &Pack, identity: Identity) {
        let approved_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let record = Record {
            config_hash: identity.hash,
            approved_at,
            config_lua: pack.lua_config().unwrap_or_default().to_string(),
        };
        self.packs.insert(identity.key, record);
    }

    /// Write the approvals to `file`. The write is atomic, and the file has
    /// mode 0600. The oldest approvals go if there are too many or the
    /// file is too large.
    fn write(&mut self, file: &VerifiedPath) -> anyhow::Result<()> {
        self.version = STORE_VERSION;
        let json = loop {
            let json = serde_json::to_vec_pretty(self)?;
            if (self.packs.len() <= MAX_RECORDS && json.len() as u64 <= STORE_CAP)
                || self.packs.len() <= 1
            {
                break json;
            }
            let oldest = self
                .packs
                .iter()
                .min_by_key(|(_, record)| record.approved_at)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                self.packs.remove(&key);
            }
        };
        write_atomic(file, &json).with_context(|| messages::packs::approvals_file_failed(file))
    }
}

/// Write `bytes` to `file` through a temporary file in the same directory
/// and a rename. The file gets mode 0600.
fn write_atomic(file: &VerifiedPath, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = file
        .parent()
        .with_context(|| messages::packs::approvals_file_failed(file))?;
    // The pid in the name keeps two processes apart.
    let tmp = dir.join(format!(".{STORE_FILE}.{}.tmp", std::process::id()))?;
    let _ = tmp.remove_file();
    let result = (|| -> anyhow::Result<()> {
        let mut out = tmp.open_with(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW),
        )?;
        out.write_all(bytes)?;
        out.sync_all()?;
        tmp.rename_to(file)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = tmp.remove_file();
    }
    result
}

/// A pack that needs the approval of the user.
struct Pending<'a> {
    /// The pack, with its config script in memory.
    pack: &'a Pack,
    /// Copy of the config script of the last approval, if the pack had one.
    last: Option<&'a str>,
}

impl Pending<'_> {
    /// Return the pack id and label, made safe for the terminal.
    fn name(&self) -> (String, String) {
        let metadata = self.pack.metadata();
        (
            style::escape_controls(metadata.id.as_str(), false).into_owned(),
            style::escape_controls(&metadata.label, false).into_owned(),
        )
    }

    /// Return the lines that show the config script to the user. A new pack
    /// shows all of the script. A changed pack shows the changes since the
    /// last approval. All text from the pack is made safe for the terminal
    /// (see [`style::escape_controls`]).
    fn lines(&self, styles: &Styles) -> Vec<String> {
        let (id, label) = self.name();
        let new = self.pack.lua_config().unwrap_or_default();
        let header = match self.last {
            Some(_) => messages::packs::approval_script_changed(&id, &label),
            None => messages::packs::approval_script_new(&id, &label),
        };
        let mut lines = vec![styles.cyan.apply_to(header).to_string()];
        lines.extend(prompt::diff::unified(
            &style::escape_controls(self.last.unwrap_or_default(), true),
            &style::escape_controls(new, true),
            styles,
        ));
        lines
    }
}

/// The texts of the approval question.
struct Question {
    /// True if a pack changed since its last approval.
    changed: bool,
    /// The question.
    title: &'static str,
    /// Label of the option that approves.
    action: &'static str,
    /// The list of the packs. If a pack changed, new packs have a mark.
    items: Vec<String>,
}

impl Question {
    /// Make the texts of the question about `pending`.
    fn new(pending: &[Pending<'_>]) -> Self {
        let changed = pending.iter().any(|p| p.last.is_some());
        let items = pending
            .iter()
            .map(|p| {
                let (id, label) = p.name();
                if changed && p.last.is_none() {
                    messages::packs::pack_item_new(id, label)
                } else {
                    messages::packs::pack_item(id, label)
                }
            })
            .collect();
        let (title, action) = if changed {
            (
                messages::packs::APPROVAL_TITLE_CHANGED,
                messages::packs::APPROVAL_CHOICE_APPROVE,
            )
        } else {
            (
                messages::packs::APPROVAL_TITLE_NEW,
                messages::packs::APPROVAL_CHOICE_CONTINUE,
            )
        };
        Self {
            changed,
            title,
            action,
            items,
        }
    }
}

/// Ask the user about `pending` in the terminal. The first answer, `Abort`,
/// is the default.
///
/// If all packs are new, the question lists them. Else the question lists
/// them, and a key shows and hides the scripts and the changes in the
/// question.
// The user must be able to see all of the code before the approval. Thus
// the details wrap long lines and scroll when they do not fit.
fn ask_terminal(pending: &[Pending<'_>]) -> anyhow::Result<Decision> {
    let Question {
        changed,
        title,
        action,
        items,
    } = Question::new(pending);
    let notes: Vec<&str> = items.iter().map(String::as_str).collect();
    let choices = [
        Choice {
            label: messages::cli::ABORT_CHOICE,
        },
        Choice { label: action },
    ];
    let question = Choose {
        title,
        notes: &notes,
        choices: &choices,
        default: 0,
        report: true,
    };
    let answer = if changed {
        let styles = Styles::new();
        let lines: Vec<String> = pending.iter().flat_map(|p| p.lines(&styles)).collect();
        question.ask_with_details(&Details {
            key: 's',
            show_hint: messages::packs::SHOW_CHANGES_HINT,
            hide_hint: messages::packs::HIDE_CHANGES_HINT,
            lines: &lines,
        })?
    } else {
        question.ask()?
    };
    match answer {
        Some(1) => Ok(Decision::Approve),
        _ => Ok(Decision::Abort),
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the approval question lines.

    use std::sync::Arc;

    use super::*;

    /// A user pack `~p` whose `config.lua` is `lua`.
    fn pack_with_lua(lua: &str) -> Pack {
        let pack_toml = b"label = \"P\"\ndescription = \"d\"\n".as_slice();
        let data = crate::packs::loader::load_pack(
            "p",
            PackId::local("p"),
            None,
            &BTreeMap::from([("pack.toml", pack_toml), ("config.lua", lua.as_bytes())]),
        )
        .unwrap();
        Pack(Arc::new(data))
    }

    /// Return `lines` with no colors.
    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|line| console::strip_ansi_codes(line).into_owned())
            .collect()
    }

    /// Test that the lines of a changed pack show the changes of the config
    /// script since the last approval, and that the lines of a new pack show
    /// all of the script. The user must see all of the code that can run.
    ///   1. Make a pack whose script changed since the approved copy
    ///   2. Check the header and the diff lines
    ///   3. Check the lines of the same pack with no approved copy
    #[test]
    fn changed_pack_shows_script_diff_and_new_pack_shows_script() {
        let pack = pack_with_lua("a = 2\nb = 1\n");
        let changed = Pending {
            pack: &pack,
            last: Some("a = 1\nb = 1\n"),
        };
        assert_eq!(
            plain(&changed.lines(&Styles::new())),
            [
                "~p (P): config.lua changed",
                "- a = 1",
                "+ a = 2",
                "  b = 1"
            ]
        );
        let new = Pending {
            pack: &pack,
            last: None,
        };
        assert_eq!(
            plain(&new.lines(&Styles::new())),
            ["~p (P): config.lua (new)", "+ a = 2", "+ b = 1"]
        );
    }

    /// Test that the question about only new packs lists them with no mark
    /// and offers `Continue`, and that the question with a changed pack
    /// marks the new packs and offers `Approve`. The user must see which
    /// packs are new.
    ///   1. Make the question about two new packs and check its texts
    ///   2. Give the first pack an approved copy and check the texts again
    #[test]
    fn question_marks_new_packs_only_when_a_pack_changed() {
        let one = pack_with_lua("a = 1\n");
        let two = pack_with_lua("b = 1\n");
        let mut pending = [
            Pending {
                pack: &one,
                last: None,
            },
            Pending {
                pack: &two,
                last: None,
            },
        ];
        let question = Question::new(&pending);
        assert!(!question.changed);
        assert_eq!(question.title, messages::packs::APPROVAL_TITLE_NEW);
        assert_eq!(question.action, messages::packs::APPROVAL_CHOICE_CONTINUE);
        assert_eq!(question.items, ["  ~p (P)", "  ~p (P)"]);

        pending[0].last = Some("a = 0\n");
        let question = Question::new(&pending);
        assert!(question.changed);
        assert_eq!(question.title, messages::packs::APPROVAL_TITLE_CHANGED);
        assert_eq!(question.action, messages::packs::APPROVAL_CHOICE_APPROVE);
        assert_eq!(question.items, ["  ~p (P)", "  ~p (P) (new)"]);
    }

    /// Test that the lines show control and bidi characters of the config
    /// script as visible escapes. A script from the project could hide code
    /// from the user or change the terminal with them.
    ///   1. Make a pack whose `config.lua` has an escape sequence, a
    ///      carriage return and a bidi override
    ///   2. Check that no line has the raw characters
    ///   3. Check that the lines show the escapes
    #[test]
    fn control_and_bidi_characters_show_as_escapes() {
        let lua = "-- \u{1b}[2Jhidden\rconfig.env = {}\n-- \u{202e}evil\n";
        let pack = pack_with_lua(lua);
        let pending = Pending {
            pack: &pack,
            last: None,
        };
        let text = pending.lines(&Styles::new()).join("\n");
        for raw in ["\u{1b}[2J", "\r", "\u{202e}"] {
            assert!(!text.contains(raw), "{text:?}");
        }
        let text = console::strip_ansi_codes(&text);
        assert!(
            text.contains("+ -- \\x1b[2Jhidden\\x0dconfig.env = {}"),
            "{text}"
        );
        assert!(text.contains("+ -- \\u{202e}evil"), "{text}");
    }
}
