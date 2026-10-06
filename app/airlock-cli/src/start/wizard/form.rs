//! The state of the setup wizard's view and what the keys do to it (the
//! lines that show it: [`crate::start::wizard::view`]).
//!
//! The view has a section per pack kind ([`KINDS`]): the distro packs
//! are a radio group, with a first row for the image of the user files
//! when they set one ([`Row::Custom`]: no distro pack); the agents and the tools are checkboxes. A
//! selected pack has its args as rows below it; an unselected one has
//! none. An arg row lists its values ([`listed_values`]: `yes` and `no`
//! for a bool); a choice arg with `other` has the other slot after them,
//! where the user types a value. Below the sections: the start bar
//! ([`Row::Start`]) with its options ([`START_CHOICES`]).
//!
//! After the packs, the capabilities: checkboxes for the clipboard
//! ([`Row::ClipboardCopy`], [`Row::ClipboardPaste`]).
//!
//! Keys: ↑/↓ move the focus (around the ends); space toggles a checkbox
//! or a bool, or selects a radio row; ←/→ go to the previous or next
//! value of an arg row or option of the start bar (no
//! wrap). Enter moves the focus to the start bar; there, it ends the
//! view with the focused option. Esc on the start bar that Enter moved
//! to goes back to the row before; else it cancels. Ctrl-C interrupts.
//!
//! The other slot: → from the last value makes it the current item, with
//! an empty text (an arg row with a custom value has it there when it
//! gets the focus). Characters go to the text, Backspace deletes the
//! last one; ← goes back to the last value and clears the text. ↑/↓ and
//! Enter leave the row only with a text that the pack's config accepts
//! (then the arg has it); an empty text keeps the focus, another one
//! keeps it with the error of the pack's config.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent};

use crate::cli::prompt::{self, Step};
use crate::config::UserImage;
use crate::config::generated::{Clipboard, Target};
use crate::packs::{ArgKind, ArgValue, Pack, PackKind, PackManager};
use crate::start::wizard::Answers;

/// The pack kinds of the sections, in pack order.
pub const KINDS: [PackKind; 3] = [PackKind::Distro, PackKind::Agent, PackKind::Tool];

/// The options of the start bar, in order.
pub const START_CHOICES: [StartChoice; 3] = [
    StartChoice::Start,
    StartChoice::StartAndShare,
    StartChoice::Cancel,
];

/// A row that the focus can be on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// The pack of `entries[i]`.
    Pack(usize),
    /// The first row of the distro group when the user files set an
    /// image: that image, no distro pack.
    Custom,
    /// Arg `a` of the pack of `entries[i]` (only while it is selected).
    Arg(usize, usize),
    /// The sandbox may write to the host clipboard.
    ClipboardCopy,
    /// The sandbox may read the host clipboard.
    ClipboardPaste,
    /// The start bar, the last row.
    Start,
}

/// An option of the start bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartChoice {
    /// Start with the config in `.airlock/airlock.toml` (local).
    Start,
    /// Start with the config in `airlock.toml` (shareable).
    StartAndShare,
    /// End without a config.
    Cancel,
}

impl StartChoice {
    /// Where the config goes; none on cancel.
    pub fn target(self) -> Option<Target> {
        match self {
            StartChoice::Start => Some(Target::Local),
            StartChoice::StartAndShare => Some(Target::Project),
            StartChoice::Cancel => None,
        }
    }
}

/// A pack of the view.
pub struct Entry {
    pub pack: Pack,
    pub selected: bool,
    /// The value of every arg of the pack, by key (the defaults at first).
    pub values: BTreeMap<String, ArgValue>,
}

impl Entry {
    pub fn is_distro(&self) -> bool {
        self.pack.metadata().kind == PackKind::Distro
    }
}

/// The other slot of the focused choice arg, while it is the current
/// item. The arg keeps its value until the focus leaves the row with the
/// text (see the module docs).
#[derive(Default)]
pub struct OtherSlot {
    pub text: String,
    /// Why the pack's config did not accept the text (until it
    /// changes).
    pub error: Option<String>,
}

impl OtherSlot {
    /// Whether the text has no value (it is blank).
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// The state of the view.
pub struct Form {
    /// Every pack offered (the newest version of each), in pack order.
    entries: Vec<Entry>,
    /// The image that the user files set ([`Row::Custom`]).
    custom_image: Option<UserImage>,
    clipboard: Clipboard,
    /// The current option of the start bar.
    start: StartChoice,
    focus: Row,
    /// The row that Enter moved the focus to the start bar from (Esc
    /// goes back there).
    return_to: Option<Row>,
    /// The other slot of the focused arg row, while it is current.
    other: Option<OtherSlot>,
    /// Why the last answers did not pass the check (see
    /// [`Form::check_failed`]).
    check_error: Option<String>,
}

impl Form {
    /// The view of `packs`: the image of the user files (`custom_image`)
    /// is selected, else the first distro pack; no agent or tool; the args have their defaults; the start bar is on
    /// [`StartChoice::Start`]. The focus is on the first row.
    pub fn new(packs: &PackManager, custom_image: Option<UserImage>) -> Self {
        let offered = packs.builtin();
        let first_distro = offered
            .iter()
            .position(|p| p.metadata().kind == PackKind::Distro);
        let entries: Vec<Entry> = offered
            .into_iter()
            .enumerate()
            .map(|(i, pack)| Entry {
                selected: custom_image.is_none() && Some(i) == first_distro,
                values: pack
                    .args()
                    .iter()
                    .map(|arg| (arg.key.clone(), arg.default.clone()))
                    .collect(),
                pack,
            })
            .collect();
        let mut form = Self {
            entries,
            custom_image,
            clipboard: Clipboard {
                copy: true,
                paste: false,
            },
            start: StartChoice::Start,
            focus: Row::Start,
            return_to: None,
            other: None,
            check_error: None,
        };
        form.focus = form.rows()[0];
        form
    }

    /// The answers: the selected packs with their arg values, in pack
    /// order, and `target`, where the config goes.
    pub fn answers(&self, target: Target) -> Answers {
        Answers {
            packs: self
                .entries
                .iter()
                .filter(|e| e.selected)
                .map(|e| e.pack.configure(&e.values))
                .collect(),
            image: self
                .custom_image
                .as_ref()
                .filter(|_| !self.entries.iter().any(|e| e.is_distro() && e.selected))
                .map(|image| image.value.clone()),
            clipboard: self.clipboard,
            target,
        }
    }

    /// The answers did not pass the check: `error` shows under the start
    /// bar (which has the focus) until the next check.
    pub fn check_failed(&mut self, error: String) {
        self.check_error = Some(error);
    }

    /// Why the last answers did not pass the check, if they did not.
    pub fn check_error(&self) -> Option<&str> {
        self.check_error.as_deref()
    }

    /// Every pack offered, in pack order ([`Row::Pack`] and [`Row::Arg`]
    /// index it).
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn focus(&self) -> Row {
        self.focus
    }

    /// The other slot of the focused arg row, while it is current.
    pub fn other(&self) -> Option<&OtherSlot> {
        self.other.as_ref()
    }

    /// The clipboard capabilities ([`Row::ClipboardCopy`],
    /// [`Row::ClipboardPaste`]).
    pub fn clipboard(&self) -> Clipboard {
        self.clipboard
    }

    /// The image that the user files set ([`Row::Custom`]).
    pub fn custom_image(&self) -> Option<&UserImage> {
        self.custom_image.as_ref()
    }

    /// The current option of the start bar.
    pub fn start(&self) -> StartChoice {
        self.start
    }

    /// The rows of the packs of `kind`: each pack, with its args while
    /// it is selected; the distro group starts with [`Row::Custom`] when
    /// the user files set an image.
    /// None without packs of `kind`.
    pub fn section_rows(&self, kind: PackKind) -> Vec<Row> {
        let mut rows = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.pack.metadata().kind != kind {
                continue;
            }
            rows.push(Row::Pack(i));
            if entry.selected {
                rows.extend((0..entry.pack.args().len()).map(|a| Row::Arg(i, a)));
            }
        }
        if kind == PackKind::Distro && self.custom_image.is_some() {
            rows.insert(0, Row::Custom);
        }
        rows
    }

    /// Apply `key` (see the module docs).
    pub fn key(&mut self, key: KeyEvent) -> Step<Target> {
        if prompt::is_interrupt_key(key) {
            return Step::Interrupt;
        }
        if let Some(other) = self.other.as_mut()
            && prompt::edit_text(&mut other.text, key)
        {
            other.error = None;
            return Step::Stay;
        }
        match key.code {
            KeyCode::Up => self.move_focus(false),
            KeyCode::Down => self.move_focus(true),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Left => self.change(false),
            KeyCode::Right => self.change(true),
            KeyCode::Enter if self.focus == Row::Start => {
                return self.start.target().map_or(Step::Cancel, Step::Done);
            }
            KeyCode::Enter => {
                let from = self.focus;
                self.go_to(Row::Start);
                if self.focus == Row::Start {
                    self.return_to = Some(from);
                }
            }
            KeyCode::Esc if self.focus == Row::Start && self.return_to.is_some() => {
                let back = self.return_to.take();
                self.go_to(back.unwrap_or(Row::Start));
            }
            KeyCode::Esc => return Step::Cancel,
            _ => {}
        }
        Step::Stay
    }

    /// The rows, top to bottom.
    fn rows(&self) -> Vec<Row> {
        KINDS
            .iter()
            .flat_map(|kind| self.section_rows(*kind))
            .chain([Row::ClipboardCopy, Row::ClipboardPaste, Row::Start])
            .collect()
    }

    /// Select the distro pack of `entries[chosen]`, or none, and no
    /// other.
    fn select_distro(&mut self, chosen: Option<usize>) {
        for (i, entry) in self.entries.iter_mut().enumerate() {
            if entry.is_distro() {
                entry.selected = Some(i) == chosen;
            }
        }
    }

    /// Move the focus to the next (`forward`) or previous row, around the
    /// ends (see [`Form::go_to`]).
    fn move_focus(&mut self, forward: bool) {
        self.return_to = None;
        let rows = self.rows();
        let at = rows.iter().position(|r| *r == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % rows.len()
        } else {
            (at + rows.len() - 1) % rows.len()
        };
        self.go_to(rows[next]);
    }

    /// Move the focus to `row`, if the focused row lets it go (see
    /// [`Form::commit_other`]). An arg row with a custom value gets it in
    /// its other slot.
    fn go_to(&mut self, row: Row) {
        if !self.commit_other() {
            return;
        }
        self.focus = row;
        self.other = self.custom_value(row).map(|text| OtherSlot {
            text: text.to_string(),
            error: None,
        });
    }

    /// Give the text of the other slot to its arg and close the slot.
    /// False (the slot stays) when the text is empty or the pack's config
    /// does not accept it (the slot has the error then); true without a
    /// slot.
    fn commit_other(&mut self) -> bool {
        let (Some(other), Row::Arg(i, a)) = (self.other.as_mut(), self.focus) else {
            return true;
        };
        if other.is_empty() {
            return false;
        }
        let entry = &mut self.entries[i];
        let mut values = entry.values.clone();
        values.insert(
            entry.pack.args()[a].key.clone(),
            ArgValue::Text(other.text.trim().to_string()),
        );
        match entry.pack.configure(&values).config_values() {
            Ok(_) => {
                entry.values = values;
                self.other = None;
                true
            }
            Err(e) => {
                other.error = Some(format!("{e:#}"));
                false
            }
        }
    }

    /// The value of the arg of `row` if it is a custom value: a text that
    /// is not one of the arg's values.
    fn custom_value(&self, row: Row) -> Option<&str> {
        let Row::Arg(i, a) = row else {
            return None;
        };
        let entry = &self.entries[i];
        let arg = &entry.pack.args()[a];
        match (&arg.kind, &entry.values[&arg.key]) {
            (ArgKind::Choice { values, .. }, ArgValue::Text(text)) if !values.contains(text) => {
                Some(text)
            }
            _ => None,
        }
    }

    /// Space: toggle the focused checkbox or bool, or select the focused
    /// radio row.
    fn toggle(&mut self) {
        match self.focus {
            Row::Pack(i) if self.entries[i].is_distro() => self.select_distro(Some(i)),
            Row::Pack(i) => self.entries[i].selected = !self.entries[i].selected,
            Row::Custom => self.select_distro(None),
            Row::Arg(i, a) => {
                let entry = &mut self.entries[i];
                let key = &entry.pack.args()[a].key;
                if let Some(ArgValue::Bool(on)) = entry.values.get_mut(key) {
                    *on = !*on;
                }
            }
            Row::ClipboardCopy => self.clipboard.copy = !self.clipboard.copy,
            Row::ClipboardPaste => self.clipboard.paste = !self.clipboard.paste,
            Row::Start => {}
        }
    }

    /// ←/→: the previous or next (`forward`) value of the focused arg
    /// (after the last one, the other slot; from the other slot, back to
    /// the last one), or option of the start bar. Nothing at the ends.
    fn change(&mut self, forward: bool) {
        match self.focus {
            Row::Arg(i, a) => {
                let entry = &mut self.entries[i];
                let arg = &entry.pack.args()[a];
                let listed = listed_values(&arg.kind);
                if self.other.is_some() {
                    if !forward && let Some(last) = listed.last() {
                        entry.values.insert(arg.key.clone(), last.clone());
                        self.other = None;
                    }
                    return;
                }
                let Some(at) = listed.iter().position(|v| *v == entry.values[&arg.key]) else {
                    return;
                };
                match neighbor(at, listed.len(), forward) {
                    Some(next) => {
                        entry.values.insert(arg.key.clone(), listed[next].clone());
                    }
                    None if forward && matches!(arg.kind, ArgKind::Choice { other: true, .. }) => {
                        self.other = Some(OtherSlot::default());
                    }
                    None => {}
                }
            }
            Row::Start => {
                let at = START_CHOICES
                    .iter()
                    .position(|c| *c == self.start)
                    .unwrap_or(0);
                if let Some(next) = neighbor(at, START_CHOICES.len(), forward) {
                    self.start = START_CHOICES[next];
                }
            }
            Row::Pack(_) | Row::Custom | Row::ClipboardCopy | Row::ClipboardPaste => {}
        }
    }
}

/// The values that the row of an arg of `kind` lists, in order: true
/// and false (`yes`, `no`) for a bool, the values of a choice (with
/// `other`, the other slot follows them).
pub fn listed_values(kind: &ArgKind) -> Vec<ArgValue> {
    match kind {
        ArgKind::Bool => vec![ArgValue::Bool(true), ArgValue::Bool(false)],
        ArgKind::Choice { values, .. } => values.iter().cloned().map(ArgValue::Text).collect(),
    }
}

/// The index after (`forward`) or before `at` of `len` items, if there
/// is one (no wrap).
fn neighbor(at: usize, len: usize, forward: bool) -> Option<usize> {
    if forward {
        (at + 1 < len).then_some(at + 1)
    } else {
        at.checked_sub(1)
    }
}
