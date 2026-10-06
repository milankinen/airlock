//! The look that every prompt shares: the styles ([`Styles`]), the marks
//! ([`radio`], [`checkbox`]), a line builder ([`Line`]) and word wrap
//! ([`wrap`], [`indented`]).
//!
//! Titles are bold, notes and hints gray, the focused label cyan, a
//! current mark or option green, errors and destructive options red
//! ([`Tone::Danger`]). Without colors (`NO_COLOR`, `CLICOLOR=0`, a
//! terminal without them), `❯` before the focused row shows the focus
//! ([`Line::lead`]), and brackets the current option (`[yes]`).

use console::{Style, measure_text_width};

/// What an option does, for its color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    /// It destroys data or ends the run: red.
    Danger,
}

/// The styles of a prompt, for stderr (no colors when it does not
/// support them).
pub struct Styles {
    /// Whether stderr shows colors (else the styles are plain).
    pub colors: bool,
    pub plain: Style,
    pub bold: Style,
    pub dim: Style,
    pub cyan: Style,
    pub green: Style,
    pub red: Style,
}

impl Styles {
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

    /// The style of a checkbox or radio mark.
    pub fn mark(&self, selected: bool) -> &Style {
        if selected { &self.green } else { &self.plain }
    }

    /// The style of a row's label: cyan when `focused`, red for
    /// [`Tone::Danger`].
    pub fn label(&self, focused: bool, tone: Tone) -> &Style {
        match tone {
            Tone::Danger => &self.red,
            Tone::Plain if focused => &self.cyan,
            Tone::Plain => &self.plain,
        }
    }
}

pub fn radio(selected: bool) -> &'static str {
    if selected { "◉" } else { "○" }
}

pub fn checkbox(selected: bool) -> &'static str {
    if selected { "■" } else { "□" }
}

/// `text` wrapped into lines of `room` columns, each `indent` columns in
/// and in `style`.
pub fn indented(text: &str, indent: usize, room: usize, style: &Style) -> Vec<String> {
    wrap(text, room.saturating_sub(indent))
        .into_iter()
        .map(|part| format!("{}{}", " ".repeat(indent), style.apply_to(part)))
        .collect()
}

/// `text` in lines of at most `width` columns, broken between words (a
/// longer word is broken anywhere).
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

/// A line of a view while it is built: the styled text and its width.
#[derive(Default)]
pub struct Line {
    pub text: String,
    pub width: usize,
    /// The column of the label, where the description and error lines
    /// start when they go below the row.
    pub indent: usize,
    /// The column of the text cursor, if it shows on this line.
    pub cursor: Option<usize>,
}

impl Line {
    pub fn push(&mut self, text: &str, style: &Style) {
        self.text.push_str(&style.apply_to(text).to_string());
        self.width += measure_text_width(text);
    }

    /// The blank start of a row whose mark or label starts `width`
    /// columns in. Without colors, its last two columns are `❯ ` on the
    /// focused row (else the focus does not show).
    pub fn lead(&mut self, width: usize, focused: bool, styles: &Styles) {
        if styles.colors {
            self.push(&" ".repeat(width), &styles.plain);
        } else {
            self.push(&" ".repeat(width.saturating_sub(2)), &styles.plain);
            self.push(if focused { "❯ " } else { "  " }, &styles.plain);
        }
    }

    /// The label starts here (see [`Line::indent`]).
    pub fn mark_indent(&mut self) {
        self.indent = self.width;
    }

    /// The text cursor shows here (see [`Line::cursor`]).
    pub fn mark_cursor(&mut self) {
        self.cursor = Some(self.width);
    }

    /// An option of a bar (`« a · b »`): the `current` one in green (or
    /// red for [`Tone::Danger`]; without colors: in brackets), another
    /// one plain.
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

    /// An item of a focused row's values or options: the `current` one
    /// in green (without colors: in brackets), with the text cursor
    /// after it if `cursor`; another one in gray.
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
