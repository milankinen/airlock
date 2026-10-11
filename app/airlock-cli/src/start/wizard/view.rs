//! Setup wizard view.
//!
//! Makes the lines that the wizard shows from the current wizard state.

use console::measure_text_width;

use crate::cli::prompt::screen::Frame;
use crate::cli::prompt::style::{self, Line, Styles};
use crate::packs::{ArgKind, ArgValue, PackKind};
use crate::start::wizard::form::{self, Form, KINDS, Mode, Row, START_CHOICES, StartChoice};
use crate::{cli, messages};

/// Make the lines of the wizard view.
///
/// The view shows, from top to bottom:
///  * An empty line, the logo and an empty line
///  * The "Template" section, if templates exist: the empty template and
///    the templates
///  * One section per pack kind: its title and its rows
///  * The "Capabilities" section: the clipboard rows
///  * The "Config" section: the row for a shareable config (not in
///    [`Mode::Template`])
///  * An empty line, and the keys for the focused row (gray, see [`keys`])
///  * The start bar
///
/// A template row has a radio mark and the template name. A pack row has a
/// radio mark (distro) or a checkbox, and a label. Its arg rows are below
/// it, with more indent, as `<label>: <value>`. The value is gray (`yes` or
/// `no` for a bool). A locked row (see [`Form::is_locked`]) is all gray. The
/// start bar shows its current option.
///
/// The focused row has a cyan label. A focused pack row has its description
/// in gray: ` - description` after the label, or on the next lines if it does
/// not fit. A focused arg row shows all its values. The current value is green
/// and the others are gray. The focused start bar shows all its options. The
/// current option is cyan and the others are plain.
/// A choice arg with `other` shows [`messages::start::WIZARD_OTHER_CHOICE`] as its last value. When
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
        cli::LOGO
            .iter()
            .map(|line| styles.bold.apply_to(line).to_string()),
    );
    lines.push(String::new());
    let mut focus = 0..0;
    let mut cursor = None;
    let share = match form.mode() {
        Mode::Start => vec![Row::ShareConfig],
        Mode::Template => Vec::new(),
    };
    let sections = std::iter::once((
        Some(messages::start::WIZARD_SECTION_TEMPLATE),
        form.template_rows(),
    ))
    .chain(
        KINDS
            .iter()
            .map(|kind| (Some(section_title(*kind)), form.section_rows(*kind))),
    )
    .chain([
        (
            Some(messages::start::WIZARD_SECTION_CAPABILITIES),
            vec![Row::ClipboardCopy, Row::ClipboardPaste],
        ),
        (Some(messages::start::WIZARD_SECTION_CONFIG), share),
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
        PackKind::Distro => messages::start::WIZARD_SECTION_BASE_IMAGE,
        PackKind::Agent => messages::start::WIZARD_SECTION_AGENTS,
        PackKind::Tool => messages::start::WIZARD_SECTION_TOOLS,
    }
}

/// Return the key help for the focused row of `form`.
fn keys(form: &Form) -> String {
    let enter = start_label(StartChoice::Start, form.mode());
    match (form.focus(), form.other()) {
        (Row::Start, _) => messages::start::WIZARD_KEYS_START_BAR.to_string(),
        (_, Some(other)) if other.is_empty() => {
            messages::start::WIZARD_KEYS_OTHER_EMPTY.to_string()
        }
        (_, Some(other)) if other.error.is_some() => {
            messages::start::WIZARD_KEYS_OTHER_ERROR.to_string()
        }
        (_, Some(_)) => messages::start::wizard_keys_other(enter),
        (Row::Arg(..), None) => messages::start::wizard_keys_arg(enter),
        (
            Row::Template(_)
            | Row::Pack(_)
            | Row::Custom
            | Row::ClipboardCopy
            | Row::ClipboardPaste
            | Row::ShareConfig,
            None,
        ) => messages::start::wizard_keys_select(enter),
    }
}

/// Return the label of a start bar option in `mode`.
fn start_label(choice: StartChoice, mode: Mode) -> &'static str {
    match (choice, mode) {
        (StartChoice::Start, Mode::Start) => messages::start::WIZARD_START_LABEL_START,
        (StartChoice::Start, Mode::Template) => messages::start::WIZARD_START_LABEL_CREATE,
        (StartChoice::Abort, _) => messages::start::WIZARD_START_LABEL_ABORT,
    }
}

/// Return the description of the template row `template`: the packs of
/// the template that the view knows.
fn template_description(form: &Form, template: Option<usize>) -> String {
    let known = |name: &str| {
        form.entries()
            .iter()
            .any(|e| e.pack.metadata().name == name)
    };
    let packs: Vec<&str> = template.map_or(Vec::new(), |t| {
        form.templates()[t]
            .config
            .packs
            .iter()
            .map(|p| p.name.as_str())
            .filter(|name| known(name))
            .collect()
    });
    if packs.is_empty() {
        messages::start::WIZARD_NO_PACKS_DESCRIPTION.to_string()
    } else {
        messages::start::wizard_template_packs(packs.join(", "))
    }
}

/// Return the display text of an arg value. A bool shows as `yes` or `no`.
fn value_text(value: &ArgValue) -> &str {
    match value {
        ArgValue::Bool(true) => messages::start::WIZARD_YES,
        ArgValue::Bool(false) => messages::start::WIZARD_NO,
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
        Row::Template(t) if focused => Some(template_description(form, t)),
        Row::Pack(i) if focused => Some(form.entries()[i].pack.metadata().description.clone()),
        Row::Custom if focused => Some(messages::start::WIZARD_CUSTOM_DESCRIPTION.to_string()),
        Row::ClipboardCopy if focused => {
            Some(messages::start::WIZARD_CLIPBOARD_COPY_DESCRIPTION.to_string())
        }
        Row::ClipboardPaste if focused => {
            Some(messages::start::WIZARD_CLIPBOARD_PASTE_DESCRIPTION.to_string())
        }
        Row::ShareConfig if focused => {
            Some(messages::start::WIZARD_SHARE_CONFIG_DESCRIPTION.to_string())
        }
        _ => None,
    };
    let error = match row {
        Row::Arg(..) if focused => form.other().and_then(|o| o.error.clone()),
        Row::Start => form.check_error().map(messages::start::config_error),
        _ => None,
    };
    let mut lines = Vec::new();
    match description {
        Some(text) if line.width + 3 + measure_text_width(&text) <= room => {
            let mut line = line;
            line.push(&format!(" - {text}"), &styles.dim);
            lines.push(line.text);
        }
        Some(text) => {
            lines.push(line.text);
            lines.extend(style::indented(&text, indent, room, &styles.dim));
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
    let locked = form.is_locked(row);
    let label = if locked {
        &styles.dim
    } else {
        styles.label(focused)
    };
    let mark = |selected| {
        if locked {
            &styles.dim
        } else {
            styles.mark(selected)
        }
    };
    match row {
        Row::Template(t) => {
            let selected = form.template() == t;
            line.lead(2, focused, styles);
            line.push(style::radio(selected), styles.mark(selected));
            line.push(" ", &styles.plain);
            line.mark_indent();
            let name = t.map_or(messages::start::WIZARD_TEMPLATE_EMPTY, |t| {
                form.templates()[t].name.as_str()
            });
            line.push(name, label);
        }
        Row::Pack(i) => {
            let entry = &form.entries()[i];
            let symbol = if entry.is_distro() {
                style::radio(entry.selected)
            } else {
                style::checkbox(entry.selected)
            };
            line.lead(2, focused, styles);
            line.push(symbol, mark(entry.selected));
            line.push(" ", &styles.plain);
            line.mark_indent();
            line.push(&entry.pack.metadata().label, label);
        }
        Row::Custom => {
            let selected = form.selected_template().is_none() && !form.distro_selected();
            line.lead(2, focused, styles);
            line.push(style::radio(selected), mark(selected));
            line.push(" ", &styles.plain);
            line.mark_indent();
            let image = form.custom_image().map_or("", |image| image.name.as_str());
            line.push(&messages::start::wizard_custom_image(image), label);
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
                    None => line.push(messages::start::WIZARD_OTHER_CHOICE, &styles.dim),
                }
            }
        }
        Row::ClipboardCopy | Row::ClipboardPaste => {
            let (name, on) = if row == Row::ClipboardCopy {
                (
                    messages::start::WIZARD_CLIPBOARD_COPY,
                    form.clipboard().copy,
                )
            } else {
                (
                    messages::start::WIZARD_CLIPBOARD_PASTE,
                    form.clipboard().paste,
                )
            };
            line.lead(2, focused, styles);
            line.push(style::checkbox(on), styles.mark(on));
            line.push(" ", &styles.plain);
            line.mark_indent();
            line.push(name, label);
        }
        Row::ShareConfig => {
            let on = form.share();
            line.lead(2, focused, styles);
            line.push(style::checkbox(on), styles.mark(on));
            line.push(" ", &styles.plain);
            line.mark_indent();
            line.push(messages::start::WIZARD_CREATE_CONFIG, label);
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
                    let text = start_label(choice, form.mode());
                    line.push_option(text, choice == form.start(), styles);
                }
            } else {
                line.push(start_label(form.start(), form.mode()), &styles.plain);
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
