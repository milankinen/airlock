//! Setup wizard state.
//!
//! Keeps the answers and the focus of the wizard view, and changes them when
//! the user presses keys.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent};

use crate::cli::prompt::{self, Step};
use crate::config::UserImage;
use crate::config::generated::{Clipboard, Target};
use crate::packs::{ArgKind, ArgValue, Pack, PackKind, PackManager};
use crate::start::wizard::Answers;

/// The pack kinds of the sections, in pack order. Each kind has one section.
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
    /// The image of the user files, with no distro pack. It is the first row
    /// of the distro group, if the user files set an image.
    Custom,
    /// Arg `a` of the pack of `entries[i]` (only when the pack is selected).
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
    /// Return where the config goes, or `None` on cancel.
    pub fn target(self) -> Option<Target> {
        match self {
            StartChoice::Start => Some(Target::Local),
            StartChoice::StartAndShare => Some(Target::Project),
            StartChoice::Cancel => None,
        }
    }
}

/// A pack in the view.
pub struct Entry {
    /// The pack (newest version).
    pub pack: Pack,
    /// True if the user selected the pack.
    pub selected: bool,
    /// The value of every arg of the pack, by key (the defaults at the start).
    pub values: BTreeMap<String, ArgValue>,
}

impl Entry {
    /// Return true if the pack is a distro pack.
    pub fn is_distro(&self) -> bool {
        self.pack.metadata().kind == PackKind::Distro
    }
}

/// The "other" slot of the focused choice arg, when it is the current item.
///
/// The user types a custom value in the slot. The arg keeps its old value
/// until the focus leaves the row with the text (see [`Form::key`]).
#[derive(Default)]
pub struct OtherSlot {
    /// The typed text.
    pub text: String,
    /// The reason why the pack config did not accept the text. Stays until
    /// the text changes.
    pub error: Option<String>,
}

impl OtherSlot {
    /// Return true if the text is blank.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// The state of the setup wizard view.
///
/// The view has one section per pack kind ([`KINDS`]):
///  * The distro packs are a radio group. If the user files set an image,
///    the first row is that image ([`Row::Custom`], no distro pack).
///  * The agents and the tools are checkboxes.
///
/// A selected pack has its args as rows below it. An unselected pack has no
/// arg rows. An arg row shows its values ([`listed_values`]). A choice arg
/// with `other` has an "other" slot after the values, where the user types a
/// value ([`OtherSlot`]). After the packs are the capabilities: checkboxes
/// for the clipboard ([`Row::ClipboardCopy`], [`Row::ClipboardPaste`]). The
/// last row is the start bar ([`Row::Start`]) with its options
/// ([`START_CHOICES`]).
pub struct Form {
    /// All offered packs (the newest version of each), in pack order.
    entries: Vec<Entry>,
    /// The image that the user files set ([`Row::Custom`]).
    custom_image: Option<UserImage>,
    clipboard: Clipboard,
    /// The current option of the start bar.
    start: StartChoice,
    focus: Row,
    /// The row from which Enter moved the focus to the start bar. Esc goes
    /// back to this row.
    return_to: Option<Row>,
    /// The "other" slot of the focused arg row, when it is current.
    other: Option<OtherSlot>,
    /// The reason why the last answers did not pass the check (see
    /// [`Form::check_failed`]).
    check_error: Option<String>,
}

impl Form {
    /// Create the start state of the view.
    /// Args:
    ///  - `packs`: Available packs. The view offers the built-in packs.
    ///  - `custom_image`: The image of the user files, if set
    ///
    /// Returns:
    ///   The state. The image of the user files is selected, or else the
    ///   first distro pack. No agent or tool is selected, and the args have
    ///   their defaults. The start bar is on [`StartChoice::Start`]. The focus
    ///   is on the first row.
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

    /// Return the answers: the selected packs with their arg values, in pack
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

    /// Record that the answers did not pass the check. `error` shows below
    /// the start bar (which has the focus) until the next check.
    pub fn check_failed(&mut self, error: String) {
        self.check_error = Some(error);
    }

    /// Return the reason why the last answers did not pass the check, if any.
    pub fn check_error(&self) -> Option<&str> {
        self.check_error.as_deref()
    }

    /// Return all offered packs, in pack order. [`Row::Pack`] and [`Row::Arg`]
    /// are indexes into this list.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Return the focused row.
    pub fn focus(&self) -> Row {
        self.focus
    }

    /// Return the "other" slot of the focused arg row, when it is current.
    pub fn other(&self) -> Option<&OtherSlot> {
        self.other.as_ref()
    }

    /// Return the clipboard capabilities ([`Row::ClipboardCopy`],
    /// [`Row::ClipboardPaste`]).
    pub fn clipboard(&self) -> Clipboard {
        self.clipboard
    }

    /// Return the image that the user files set ([`Row::Custom`]).
    pub fn custom_image(&self) -> Option<&UserImage> {
        self.custom_image.as_ref()
    }

    /// Return the current option of the start bar.
    pub fn start(&self) -> StartChoice {
        self.start
    }

    /// Return the rows of the packs of `kind`.
    ///
    /// Each pack has a row, followed by its arg rows if it is selected. If
    /// the user files set an image, the distro group starts with
    /// [`Row::Custom`]. Empty if there are no packs of `kind`.
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

    /// Apply a key press to the state.
    ///
    /// Keys:
    ///  * ↑/↓: Move the focus (wraps at the ends).
    ///  * Space: Toggle a checkbox or a bool, or select a radio row.
    ///  * ←/→: Go to the previous or next value of an arg row, or option of
    ///    the start bar (no wrap).
    ///  * Enter: Move the focus to the start bar. On the start bar, end the
    ///    view with the focused option.
    ///  * Esc: On the start bar after Enter, go back to the earlier row.
    ///    Otherwise cancel.
    ///  * Ctrl+C: Interrupt.
    ///
    /// The "other" slot: → from the last value makes it the current item,
    /// with an empty text. An arg row with a custom value has the value in
    /// the slot when it gets the focus. Characters go to the text and
    /// Backspace deletes the last character. ← goes back to the last value
    /// and clears the text. ↑/↓ and Enter leave the row only if the pack
    /// config accepts the text. Then the arg gets the text. An empty text
    /// keeps the focus. A text that the pack config does not accept keeps the
    /// focus and shows the error.
    /// Returns:
    ///   The next [`Step`]. `Done` contains the target of the config.
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

    /// Return all rows, top to bottom.
    fn rows(&self) -> Vec<Row> {
        KINDS
            .iter()
            .flat_map(|kind| self.section_rows(*kind))
            .chain([Row::ClipboardCopy, Row::ClipboardPaste, Row::Start])
            .collect()
    }

    /// Select the distro pack of `entries[chosen]`, or no distro pack. Clear
    /// all other distro selections.
    fn select_distro(&mut self, chosen: Option<usize>) {
        for (i, entry) in self.entries.iter_mut().enumerate() {
            if entry.is_distro() {
                entry.selected = Some(i) == chosen;
            }
        }
    }

    /// Move the focus to the next (`forward`) or previous row. Wraps at the
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

    /// Move the focus to `row`, if the focused row allows it (see
    /// [`Form::commit_other`]). An arg row with a custom value gets the value
    /// in its "other" slot.
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

    /// Give the text of the "other" slot to its arg and close the slot.
    /// Returns:
    ///   False if the text is empty or the pack config does not accept it.
    ///   Then the slot stays open, with the error in the second case. True if
    ///   the commit succeeded or there is no slot.
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

    /// Return the value of the arg of `row` if it is a custom value: a text
    /// that is not one of the listed values of the arg.
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

    /// ←/→: Go to the previous or next (`forward`) value of the focused arg,
    /// or option of the start bar. Do nothing at the ends.
    ///
    /// After the last value of a choice arg with `other`, → goes to the
    /// "other" slot. From the slot, ← goes back to the last value.
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

/// Return the values that the row of an arg of `kind` shows, in order.
///
/// For a bool: true and false (`yes`, `no`). For a choice: its values. With
/// `other`, the "other" slot comes after them.
pub fn listed_values(kind: &ArgKind) -> Vec<ArgValue> {
    match kind {
        ArgKind::Bool => vec![ArgValue::Bool(true), ArgValue::Bool(false)],
        ArgKind::Choice { values, .. } => values.iter().cloned().map(ArgValue::Text).collect(),
    }
}

/// Return the index after (`forward`) or before `at` in `len` items, if it
/// exists (no wrap).
fn neighbor(at: usize, len: usize, forward: bool) -> Option<usize> {
    if forward {
        (at + 1 < len).then_some(at + 1)
    } else {
        at.checked_sub(1)
    }
}
