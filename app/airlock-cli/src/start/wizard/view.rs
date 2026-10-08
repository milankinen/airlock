//! Setup wizard view.
//!
//! Makes the lines that the wizard shows from the current wizard state.

use console::measure_text_width;

use crate::cli;
use crate::cli::prompt::screen::Frame;
use crate::cli::prompt::style::{self, Line, Styles, Tone};
use crate::packs::{ArgKind, ArgValue, PackKind};
use crate::start::wizard::form::{self, Form, KINDS, Row, START_CHOICES, StartChoice};

/// The logo, at the top of the view.
const LOGO: [&str; 4] = [
    "   ▗    ▜       ▌",
    "▝▀▖▄ ▙▀▖▐ ▞▀▖▞▀▖▌▗▘",
    "▞▀▌▐ ▌  ▐ ▌ ▌▌ ▖▛▚",
    "▝▀▘▀▘▘   ▘▝▀ ▝▀ ▘ ▘",
];

/// The description of [`Row::Custom`].
const CUSTOM_DESCRIPTION: &str = "User defined image";

/// The item of the "other" slot of a choice arg, when it is not current.
const OTHER_CHOICE: &str = "other…";

/// Make the lines of the wizard view.
///
/// The view shows, from top to bottom:
///  * An empty line, the logo and an empty line
///  * One section per pack kind: its title and its rows
///  * The "Capabilities" section: the clipboard rows
///  * An empty line, and the keys for the focused row (gray, see [`keys`])
///  * The start bar
///
/// A pack row has a radio mark (distro) or a checkbox, and a label. Its arg
/// rows are below it, with more indent, as `<label>: <value>`. The value is
/// gray (`yes` or `no` for a bool). The start bar shows its current option.
///
/// The focused row has a cyan label. A focused pack row has its description
/// in gray: ` - description` after the label, or on the next lines if it does
/// not fit. A focused arg row shows all its values, and the focused start bar
/// shows all its options. The current one is green and the others are gray.
/// A choice arg with `other` shows [`OTHER_CHOICE`] as its last value. When
/// the "other" slot is current, its text shows in green with the text cursor
/// after it. The error of the "other" slot shows in red below its row. The
/// error of the last check shows below the start bar.
///
/// Without colors (`NO_COLOR`, `CLICOLOR=0`, or a terminal without colors), a
/// `❯` before the mark or label of the focused row shows the focus. Brackets
/// show the current value (`[lts]`).
/// Args:
///  - `form`: Wizard state
///  - `room`: Number of text columns of the terminal
///
/// Returns:
///   The frame. The focus covers the focused row with its description and
///   errors. On the first row of a section, it also covers the section title
///   (the logo for the first section, the keys for the start bar).
pub fn frame(form: &Form, room: usize) -> Frame {
    let styles = Styles::new();
    let mut lines = vec![String::new()];
    lines.extend(
        LOGO.iter()
            .map(|line| styles.bold.apply_to(line).to_string()),
    );
    lines.push(String::new());
    let mut focus = 0..0;
    let mut cursor = None;
    let sections = KINDS
        .iter()
        .map(|kind| (Some(section_title(*kind)), form.section_rows(*kind)))
        .chain([
            (
                Some("Capabilities"),
                vec![Row::ClipboardCopy, Row::ClipboardPaste],
            ),
            (None, vec![Row::Start]),
        ])
        .filter(|(_, rows)| !rows.is_empty());
    for (s, (title, rows)) in sections.enumerate() {
        let section_start = if s == 0 { 0 } else { lines.len() };
        if let Some(title) = title {
            lines.push(styles.bold.apply_to(title).to_string());
        } else {
            // The start bar has an empty line and the keys before it.
            lines.push(String::new());
            lines.push(styles.dim.apply_to(keys(form)).to_string());
        }
        for (n, row) in rows.into_iter().enumerate() {
            let start = if n == 0 { section_start } else { lines.len() };
            let first_line = lines.len();
            let shown = row_lines(form, row, room, &styles);
            lines.extend(shown.lines);
            if row == form.focus() {
                cursor = shown.cursor.map(|column| (first_line, column));
                focus = start..lines.len();
            }
        }
    }
    Frame {
        lines,
        focus,
        cursor,
    }
}

/// Return the section title for the packs of `kind`.
fn section_title(kind: PackKind) -> &'static str {
    match kind {
        PackKind::Distro => "Base image",
        PackKind::Agent => "Coding agents",
        PackKind::Tool => "Tools",
    }
}

/// Return the key help for the focused row of `form`.
fn keys(form: &Form) -> &'static str {
    match (form.focus(), form.other()) {
        (Row::Start, _) => "←→ choose · enter confirm",
        (_, Some(other)) if other.is_empty() => "type a value · ← back",
        (_, Some(other)) if other.error.is_some() => "edit the value · ← back",
        (_, Some(_)) => "↑↓ move · ← back · enter start",
        (Row::Arg(..), None) => "↑↓ move · ←→ change · enter start",
        (Row::Pack(_) | Row::Custom | Row::ClipboardCopy | Row::ClipboardPaste, None) => {
            "↑↓ move · space select · enter start"
        }
    }
}

/// Return the label of a start bar option.
fn start_label(choice: StartChoice) -> &'static str {
    match choice {
        StartChoice::Start => "start",
        StartChoice::StartAndShare => "start and share",
        StartChoice::Cancel => "cancel",
    }
}

/// Return the display text of an arg value. A bool shows as `yes` or `no`.
fn value_text(value: &ArgValue) -> &str {
    match value {
        ArgValue::Bool(true) => "yes",
        ArgValue::Bool(false) => "no",
        ArgValue::Text(text) => text,
    }
}

/// Return the lines of `row`: the row, then its errors.
///
/// A focused row also shows its description. If the description does not
/// fit in `room`, it goes on the next lines.
fn row_lines(form: &Form, row: Row, room: usize, styles: &Styles) -> Shown {
    let focused = row == form.focus();
    let line = row_line(form, row, focused, styles);
    let indent = line.indent;
    let cursor = line.cursor;
    let description = match row {
        Row::Pack(i) if focused => Some(form.entries()[i].pack.metadata().description.as_str()),
        Row::Custom if focused => Some(CUSTOM_DESCRIPTION),
        Row::ClipboardCopy if focused => Some("Sandbox can copy to the host clipboard"),
        Row::ClipboardPaste if focused => {
            Some("Sandbox can read the host clipboard. Enable only if you know what you are doing")
        }
        _ => None,
    };
    let error = match row {
        Row::Arg(..) if focused => form.other().and_then(|o| o.error.clone()),
        Row::Start => form.check_error().map(|e| format!("Config error: {e}")),
        _ => None,
    };
    let mut lines = Vec::new();
    match description {
        Some(text) if line.width + 3 + measure_text_width(text) <= room => {
            let mut line = line;
            line.push(&format!(" - {text}"), &styles.dim);
            lines.push(line.text);
        }
        Some(text) => {
            lines.push(line.text);
            lines.extend(style::indented(text, indent, room, &styles.dim));
        }
        None => lines.push(line.text),
    }
    for error in error.iter().flat_map(|e| e.lines()) {
        lines.extend(style::indented(error, indent, room, &styles.red));
    }
    Shown { lines, cursor }
}

/// Return the line of `row`, without description and errors.
fn row_line(form: &Form, row: Row, focused: bool, styles: &Styles) -> Line {
    let mut line = Line::default();
    let label = styles.label(focused, Tone::Plain);
    match row {
        Row::Pack(i) => {
            let entry = &form.entries()[i];
            let mark = if entry.is_distro() {
                style::radio(entry.selected)
            } else {
                style::checkbox(entry.selected)
            };
            line.lead(2, focused, styles);
            line.push(mark, styles.mark(entry.selected));
            line.push(" ", &styles.plain);
            line.mark_indent();
            line.push(&entry.pack.metadata().label, label);
        }
        Row::Custom => {
            let selected = !form.entries().iter().any(|e| e.is_distro() && e.selected);
            line.lead(2, focused, styles);
            line.push(style::radio(selected), styles.mark(selected));
            line.push(" ", &styles.plain);
            line.mark_indent();
            let image = form.custom_image().map_or("", |image| image.name.as_str());
            line.push(&format!("custom ({image})"), label);
        }
        Row::Arg(i, a) => {
            let entry = &form.entries()[i];
            let arg = &entry.pack.args()[a];
            let value = &entry.values[&arg.key];
            line.lead(4, focused, styles);
            line.push(&cli::bullet(), &styles.dim);
            line.push(" ", &styles.plain);
            line.mark_indent();
            line.push(&format!("{}:", arg.description), label);
            if !focused {
                line.push(" ", &styles.plain);
                line.push(value_text(value), &styles.dim);
                return line;
            }
            let other = form.other();
            for item in form::listed_values(&arg.kind) {
                line.push(" ", &styles.plain);
                let current = other.is_none() && item == *value;
                line.push_item(value_text(&item), current, false, styles);
            }
            if matches!(arg.kind, ArgKind::Choice { other: true, .. }) {
                line.push(" ", &styles.plain);
                match other {
                    Some(other) => line.push_item(&other.text, true, true, styles),
                    None => line.push(OTHER_CHOICE, &styles.dim),
                }
            }
        }
        Row::ClipboardCopy | Row::ClipboardPaste => {
            let (name, on) = if row == Row::ClipboardCopy {
                ("clipboard: copy", form.clipboard().copy)
            } else {
                ("clipboard: paste", form.clipboard().paste)
            };
            line.lead(2, focused, styles);
            line.push(style::checkbox(on), styles.mark(on));
            line.push(" ", &styles.plain);
            line.mark_indent();
            line.push(name, label);
        }
        Row::Start => {
            line.lead(0, focused, styles);
            line.mark_indent();
            line.push("« ", &styles.plain);
            if focused {
                for (n, choice) in START_CHOICES.into_iter().enumerate() {
                    if n > 0 {
                        line.push(" · ", &styles.plain);
                    }
                    let tone = if choice == StartChoice::Cancel {
                        Tone::Danger
                    } else {
                        Tone::Plain
                    };
                    line.push_option(start_label(choice), choice == form.start(), tone, styles);
                }
            } else {
                line.push(start_label(form.start()), &styles.plain);
            }
            line.push(" »", &styles.plain);
        }
    }
    line
}

/// The lines of a row, and the column of the text cursor on its first
/// line.
struct Shown {
    lines: Vec<String>,
    cursor: Option<usize>,
}
