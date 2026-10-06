//! The lines that show the setup wizard's view ([`frame`]) of a
//! [`Form`].
//!
//! An empty line, the logo and an empty line, a section per pack kind
//! (its title and its rows), an empty line, the keys that work on the
//! focused row (gray, see [`keys`]), and the start bar last. A pack
//! row has its radio mark (distro) or checkbox and its label; its arg
//! rows go below it, further in, as `<label>: <value>` with the value
//! in gray (a bool's value is `yes` or `no`). The start bar shows its
//! current option. The focused row has its label in cyan; a focused
//! pack row has its description in gray: ` - description` after the
//! label, or on the next lines when it does not fit. A focused arg row
//! lists all its values, and the start bar all its options: the current
//! one in green, the others in gray. A choice arg with `other` ends its
//! values with [`OTHER_CHOICE`] or, while the other slot is current,
//! its text in green with the text cursor after it. Without colors
//! (`NO_COLOR`, `CLICOLOR=0`, a terminal without them), `❯` before the
//! focused row's mark or label shows the focus, and brackets the
//! current value (`[lts]`). The error of the other slot shows in red
//! under its row; the error of the last check under the start bar.

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

/// The item of the other slot of a choice arg while it is not current.
const OTHER_CHOICE: &str = "other…";

/// The lines of the view of `form` for a terminal with `room` columns
/// for text. The focus covers the focused row with its description and
/// errors; on the first row of a section, the section title too (on the
/// first section, the logo; on the start bar, the keys).
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
            // The start bar: an empty line and the keys before it.
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

/// The title of the section of the packs of `kind`.
fn section_title(kind: PackKind) -> &'static str {
    match kind {
        PackKind::Distro => "Base image",
        PackKind::Agent => "Coding agents",
        PackKind::Tool => "Tools",
    }
}

/// The keys that work on the focused row of `form`.
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

/// The label of a start bar option.
fn start_label(choice: StartChoice) -> &'static str {
    match choice {
        StartChoice::Start => "start",
        StartChoice::StartAndShare => "start and share",
        StartChoice::Cancel => "cancel",
    }
}

/// How an arg value shows: a bool as `yes` or `no`.
fn value_text(value: &ArgValue) -> &str {
    match value {
        ArgValue::Bool(true) => "yes",
        ArgValue::Bool(false) => "no",
        ArgValue::Text(text) => text,
    }
}

/// The lines of `row`: the row (focused: with its description, on the
/// next lines when it does not fit in `room`), then its errors.
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

/// The line of `row` (without description and errors).
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
