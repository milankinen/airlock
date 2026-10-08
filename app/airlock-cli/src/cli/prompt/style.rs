//! Shared look of the prompts.
//!
//! Titles are bold. Notes and hints are gray. The focused label is cyan. A
//! current mark or option is green. Errors and destructive options are red.
//! Without colors (`NO_COLOR`, `CLICOLOR=0`, or a terminal without colors), a
//! `❯` before the focused row shows the focus. Brackets show the current option
//! (`[yes]`). Also wraps long text to the terminal width.

use console::{Style, measure_text_width};

/// Kind of an option. Sets the color of the option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// A normal option.
    Plain,
    /// The option destroys data or ends the run. Shows in red.
    Danger,
}

/// The styles of a prompt on stderr. No colors if stderr does not support
/// them.
pub struct Styles {
    /// True if stderr shows colors. If false, all styles are plain.
    pub colors: bool,
    /// No style.
    pub plain: Style,
    /// Bold text, for titles.
    pub bold: Style,
    /// Gray text, for notes and hints.
    pub dim: Style,
    /// Cyan text, for the focused label.
    pub cyan: Style,
    /// Green text, for the current mark or option.
    pub green: Style,
    /// Red text, for errors and destructive options.
    pub red: Style,
}

impl Styles {
    /// Create the styles for stderr.
    pub fn new() -> Self {
        let style = || Style::new().for_stderr();
        Self {
            colors: console::colors_enabled_stderr(),
            plain: style(),
            bold: style().bold(),
            dim: style().dim(),
            cyan: style().cyan(),
            green: style().green(),
            red: style().red(),
        }
    }

    /// Return the style of a checkbox or radio mark.
    pub fn mark(&self, selected: bool) -> &Style {
        if selected { &self.green } else { &self.plain }
    }

    /// Return the style of a row label: cyan when `focused`, red for
    /// [`Tone::Danger`].
    pub fn label(&self, focused: bool, tone: Tone) -> &Style {
        match tone {
            Tone::Danger => &self.red,
            Tone::Plain if focused => &self.cyan,
            Tone::Plain => &self.plain,
        }
    }
}

/// Return the radio mark for the `selected` state.
pub fn radio(selected: bool) -> &'static str {
    if selected { "◉" } else { "○" }
}

/// Return the checkbox mark for the `selected` state.
pub fn checkbox(selected: bool) -> &'static str {
    if selected { "■" } else { "□" }
}

/// Wrap `text` into styled and indented lines.
/// Args:
///  - `text`: Text to wrap
///  - `indent`: Indent of each line, in columns
///  - `room`: Total line width in columns, including the indent
///  - `style`: Style of the text
///
/// Returns:
///   The lines.
pub fn indented(text: &str, indent: usize, room: usize, style: &Style) -> Vec<String> {
    wrap(text, room.saturating_sub(indent))
        .into_iter()
        .map(|part| format!("{}{}", " ".repeat(indent), style.apply_to(part)))
        .collect()
}

/// Wrap `text` into lines of at most `width` columns.
///
/// Lines break between words. A word longer than `width` breaks at any
/// character.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && measure_text_width(&line) + 1 + measure_text_width(word) > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
        while measure_text_width(&line) > width {
            let split = line
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|i| measure_text_width(&line[..*i]) <= width)
                .last()
                .filter(|i| *i > 0)
                .unwrap_or_else(|| line.chars().next().map_or(0, char::len_utf8));
            let rest = line.split_off(split);
            lines.push(std::mem::replace(&mut line, rest));
        }
    }
    lines.push(line);
    lines
}

/// Builder for one line of a view.
#[derive(Default)]
pub struct Line {
    /// Styled text of the line.
    pub text: String,
    /// Visible width of the text in columns.
    pub width: usize,
    /// The column of the label. Description and error lines below the row
    /// start at this column.
    pub indent: usize,
    /// The column of the text cursor, if it shows on this line.
    pub cursor: Option<usize>,
}

impl Line {
    /// Add `text` in `style` to the end of the line.
    pub fn push(&mut self, text: &str, style: &Style) {
        self.text.push_str(&style.apply_to(text).to_string());
        self.width += measure_text_width(text);
    }

    /// Add the blank start of a row, `width` columns wide.
    ///
    /// The mark or label of the row starts after it. Without colors, the
    /// last two columns are `❯ ` on the focused row. Otherwise the focus does
    /// not show.
    pub fn lead(&mut self, width: usize, focused: bool, styles: &Styles) {
        if styles.colors {
            self.push(&" ".repeat(width), &styles.plain);
        } else {
            self.push(&" ".repeat(width.saturating_sub(2)), &styles.plain);
            self.push(if focused { "❯ " } else { "  " }, &styles.plain);
        }
    }

    /// Mark the current end of the line as the label start (see
    /// [`Line::indent`]).
    pub fn mark_indent(&mut self) {
        self.indent = self.width;
    }

    /// Mark the current end of the line as the text cursor position (see
    /// [`Line::cursor`]).
    pub fn mark_cursor(&mut self) {
        self.cursor = Some(self.width);
    }

    /// Add an option of a bar (`« a · b »`).
    ///
    /// The `current` option is green, or red for [`Tone::Danger`]. Without
    /// colors, it is in brackets. Other options are plain.
    pub fn push_option(&mut self, text: &str, current: bool, tone: Tone, styles: &Styles) {
        if !current {
            self.push(text, &styles.plain);
        } else if !styles.colors {
            self.push(&format!("[{text}]"), &styles.plain);
        } else if tone == Tone::Danger {
            self.push(text, &styles.red);
        } else {
            self.push(text, &styles.green);
        }
    }

    /// Add an item of the values or options of a focused row.
    ///
    /// The `current` item is green (in brackets without colors). If `cursor`
    /// is true, the text cursor goes after it. Other items are gray.
    pub fn push_item(&mut self, text: &str, current: bool, cursor: bool, styles: &Styles) {
        if !current {
            self.push(text, &styles.dim);
            return;
        }
        if !styles.colors {
            self.push("[", &styles.plain);
        }
        self.push(text, &styles.green);
        if cursor {
            self.mark_cursor();
        }
        if !styles.colors {
            self.push("]", &styles.plain);
        }
    }
}
