//! Single-choice prompt.
//!
//! Lets the user select one option from a vertical radio list.

use std::cell::Cell;
use std::ops::Range;

use console::measure_text_width;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::cli::prompt::screen::Frame;
use crate::cli::prompt::style::{self, Line, Styles};
use crate::cli::prompt::{self, PromptError, Step};
use crate::messages;

/// One option of the list.
pub struct Choice<'a> {
    /// Option text.
    pub label: &'a str,
}

/// Lines that a key shows and hides, below the notes of a [`Choose`].
///
/// Long lines wrap into more rows. If the question does not fit on the
/// screen, a window of the rows shows, and PgUp/PgDn, Home and End scroll
/// it. Thus the user can see all of each line.
pub struct Details<'a> {
    /// The key that shows and hides the lines.
    pub key: char,
    /// Gray hint while the lines are hidden, for example
    /// `Press s to show the changes`.
    pub show_hint: &'a str,
    /// Gray hint while the lines show.
    pub hide_hint: &'a str,
    /// The lines, with their styles. The text must be safe to show (see
    /// [`style::escape_controls`]).
    pub lines: &'a [String],
}

/// A prompt that selects one option from a vertical radio list.
///
/// The view shows:
///  * The title (bold)
///  * The notes (gray, indented by 2 columns)
///  * With [`Choose::ask_with_details`]: the details hint (gray, no indent)
///    and the details lines while they show. If they do not fit, a gray
///    line below them tells which lines show.
///  * One option per line. The focused option has a green `◉` and a cyan
///    label.
///  * An empty line and the key help (gray)
///
/// Keys: ↑/↓ move the focus (and wrap at the ends). Enter selects the focused
/// option. Esc aborts. Ctrl+C interrupts.
pub struct Choose<'a> {
    /// Question text (bold). Can have many lines.
    pub title: &'a str,
    /// Gray lines between the title and the options.
    pub notes: &'a [&'a str],
    /// Options of the list.
    pub choices: &'a [Choice<'a>],
    /// Index of the option that has the focus at the start.
    pub default: usize,
    /// If true, the title and the selected option stay on the terminal.
    pub report: bool,
}

impl Choose<'_> {
    /// Ask the question.
    /// Returns:
    ///   Index of the selected option, `None` on Esc, or error.
    pub fn ask(&self) -> Result<Option<usize>, PromptError> {
        self.ask_inner(None)
    }

    /// Ask the question with `details`. The hint of `details` shows below
    /// the notes, without indent. The key of `details` shows and hides its
    /// lines between the hint and the options. The lines are hidden at the
    /// start.
    /// Returns:
    ///   Index of the selected option, `None` on Esc, or error.
    pub fn ask_with_details(&self, details: &Details) -> Result<Option<usize>, PromptError> {
        self.ask_inner(Some(details))
    }

    fn ask_inner(&self, details: Option<&Details>) -> Result<Option<usize>, PromptError> {
        let mut state = State::new(
            self.default.min(self.choices.len().saturating_sub(1)),
            self.choices.len(),
        );
        let picked = prompt::run(
            &mut state,
            |state, room, height| self.frame(state, room, height, details),
            |state, key| state.on_key(key, details),
        )?;
        if let Some(i) = picked
            && self.report
        {
            prompt::report(self.title, self.choices[i].label);
        }
        Ok(picked)
    }

    /// Return the lines of the list in `state`, with the optional
    /// `details`, for `room` columns of text and `height` rows.
    fn frame(&self, state: &State, room: usize, height: usize, details: Option<&Details>) -> Frame {
        let styles = Styles::new();
        let title: Vec<String> = self
            .title
            .lines()
            .flat_map(|line| style::indented(line, 0, room, &styles.bold))
            .collect();
        let notes: Vec<String> = self
            .notes
            .iter()
            .flat_map(|note| style::indented(note, 0, room, &styles.dim))
            .collect();
        let hint = details.map_or_else(Vec::new, |details| {
            let hint = if state.details {
                details.hide_hint
            } else {
                details.show_hint
            };
            style::indented(hint, 0, room, &styles.dim)
        });
        let choices: Vec<String> = self
            .choices
            .iter()
            .enumerate()
            .map(|(i, choice)| {
                let focused = i == state.focus;
                let mut line = Line::default();
                line.lead(2, focused, &styles);
                line.push(style::radio(focused), styles.mark(focused));
                line.push(" ", &styles.plain);
                line.push(choice.label, styles.label(focused));
                line.text
            })
            .collect();
        let keys = [
            String::new(),
            styles.dim.apply_to(messages::cli::CHOOSE_KEYS).to_string(),
        ];
        let mut lines = [title.clone(), notes.clone(), hint.clone()].concat();
        let Some(details) = details.filter(|_| state.details) else {
            return choice_frame(lines, &choices, state.focus, &keys);
        };
        let rows = state.detail_rows(details.lines, room);
        if lines.len() + rows.len() + choices.len() + keys.len() <= height {
            state.view.set(View {
                rows: rows.len(),
                total: rows.len(),
                room,
            });
            state.top.set(0);
            lines.extend(rows);
            return choice_frame(lines, &choices, state.focus, &keys);
        }
        // Not all rows fit. Find the first layout that has room for one
        // detail row and the status line. Each layout drops more of the
        // other parts. Thus the frame is never taller than the terminal, and
        // the window can show each detail row. The screen cuts the status
        // line if it is too long, so it is always one row.
        let focused: Vec<String> = choices.get(state.focus).cloned().into_iter().collect();
        // Each layout: the lines above the details, whether the status line
        // shows, and the lines below the status line.
        let layouts = [
            (lines, true, [choices.clone(), keys.to_vec()].concat()),
            ([title, hint].concat(), true, choices),
            (Vec::new(), true, focused),
            (Vec::new(), true, Vec::new()),
            (Vec::new(), false, Vec::new()),
        ];
        let (mut lines, status, below) = layouts
            .into_iter()
            .find(|(above, status, below)| {
                above.len() + usize::from(*status) + below.len() < height
            })
            .unwrap_or_default();
        let view = View {
            rows: height
                .saturating_sub(lines.len() + usize::from(status) + below.len())
                .max(1),
            total: rows.len(),
            room,
        };
        state.view.set(view);
        let shown = view.window(state.top.get());
        state.top.set(shown.start);
        let text = messages::cli::details_window(shown.start + 1, shown.end, view.total);
        lines.extend(rows[shown].iter().cloned());
        if status {
            lines.push(styles.dim.apply_to(text).to_string());
        }
        lines.extend(below);
        // The frame fits on the screen, so the screen does not scroll and
        // the focus range has no effect.
        Frame {
            lines,
            focus: 0..0,
            cursor: None,
        }
    }
}

/// Return a frame with the `lines` above the `choices`, and the `keys`
/// below them. The option at `focus` has the focus.
fn choice_frame(
    mut lines: Vec<String>,
    choices: &[String],
    focus: usize,
    keys: &[String],
) -> Frame {
    // On the first option, the lines above it stay on the screen too.
    let focus = match focus {
        _ if choices.is_empty() => 0..0,
        0 => 0..lines.len() + 1,
        i => lines.len() + i..lines.len() + i + 1,
    };
    lines.extend(choices.iter().cloned());
    lines.extend(keys.iter().cloned());
    Frame {
        lines,
        focus,
        cursor: None,
    }
}

/// Ask the user to confirm a dangerous action. The options are `Abort`
/// (the default) and `action`. The question and the answer stay on
/// the terminal.
/// Args:
///  - `title`: The question
///  - `notes`: Gray lines below the question
///  - `action`: Label of the option that confirms
///
/// Returns:
///   True if the user selected `action`. False on `Abort` or Esc.
pub fn confirm(title: &str, notes: &[&str], action: &str) -> Result<bool, PromptError> {
    let question = Choose {
        title,
        notes,
        choices: &[
            Choice {
                label: messages::cli::ABORT_CHOICE,
            },
            Choice { label: action },
        ],
        default: 0,
        report: true,
    };
    Ok(question.ask()? == Some(1))
}

/// Number of columns between two tab stops.
const TAB_WIDTH: usize = 8;

/// Reset of all text styles.
const RESET: &str = "\x1b[0m";

/// Wrap the styled `line` into rows of at most `width` columns.
///
/// Breaks at any character, so that the rows show the exact text. A tab
/// becomes spaces up to the next tab stop. A character that is wider than
/// `width` shows as `\u{fffd}`. Thus no row is wider than `width`. Each
/// row ends with a style reset, and the next row starts with the styles
/// that were active.
fn wrap_styled(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    // Style codes after the last reset.
    let mut active = String::new();
    let mut buf = [0; 4];
    for (part, is_code) in console::AnsiCodeIterator::new(line) {
        if is_code {
            if part == RESET || part == "\x1b[m" {
                active.clear();
            } else {
                active.push_str(part);
            }
            row.push_str(part);
            continue;
        }
        for c in part.chars() {
            let tab = c == '\t';
            let mut width_of_char = measure_text_width(c.encode_utf8(&mut buf));
            let c = if !tab && width_of_char > width {
                width_of_char = 1;
                '\u{fffd}'
            } else {
                c
            };
            let full = if tab {
                used >= width
            } else {
                used > 0 && used + width_of_char > width
            };
            if full {
                if !active.is_empty() {
                    row.push_str(RESET);
                }
                rows.push(std::mem::replace(&mut row, active.clone()));
                used = 0;
            }
            let columns = if tab {
                // A tab at the end of a row fills the row, as in a terminal.
                let columns = (TAB_WIDTH - used % TAB_WIDTH).min(width - used);
                row.push_str(&" ".repeat(columns));
                columns
            } else {
                row.push(c);
                width_of_char
            };
            used += columns;
        }
    }
    rows.push(row);
    rows
}

/// The details window of the last frame.
#[derive(Clone, Copy, Default)]
struct View {
    /// Number of detail rows that show.
    rows: usize,
    /// Number of all detail rows.
    total: usize,
    /// Number of text columns of the rows.
    room: usize,
}

impl View {
    /// Return the largest first row.
    fn max_top(self) -> usize {
        self.total.saturating_sub(self.rows)
    }

    /// Return the detail rows that show if `top` is the first row. A `top`
    /// after the end shows the last rows.
    fn window(self, top: usize) -> Range<usize> {
        let start = top.min(self.max_top());
        start..(start + self.rows).min(self.total)
    }
}

/// The focused option in a list of `len` options, whether the details
/// show, and the scroll position of the details.
struct State {
    focus: usize,
    len: usize,
    details: bool,
    /// The first detail row that shows. The frame changes it after a
    /// resize.
    top: Cell<usize>,
    /// The details window of the last frame. The frame sets it, because
    /// only the frame knows the terminal size.
    view: Cell<View>,
}

impl State {
    fn new(focus: usize, len: usize) -> Self {
        Self {
            focus,
            len,
            details: false,
            top: Cell::new(0),
            view: Cell::default(),
        }
    }

    /// Wrap the detail `lines` into rows of `room` columns. If `room` is
    /// not the width of the last frame, move the top row to the start of
    /// the line that was at the top.
    fn detail_rows(&self, lines: &[String], room: usize) -> Vec<String> {
        let rows: Vec<Vec<String>> = lines.iter().map(|line| wrap_styled(line, room)).collect();
        let old = self.view.get();
        if old.room != 0 && old.room != room {
            let top = self.top.get();
            let mut start = 0;
            let line = lines
                .iter()
                .position(|line| {
                    start += wrap_styled(line, old.room).len();
                    start > top
                })
                .unwrap_or(lines.len());
            self.top.set(rows[..line].iter().map(Vec::len).sum());
        }
        rows.into_iter().flatten().collect()
    }

    /// Apply `key`, with the optional `details` of the question. The
    /// details key works in lower and upper case.
    fn on_key(&mut self, key: KeyEvent, details: Option<&Details>) -> Step<usize> {
        let Some(details) = details else {
            return self.key(key);
        };
        if let KeyCode::Char(c) = key.code
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && c.eq_ignore_ascii_case(&details.key)
        {
            self.details = !self.details;
            self.top.set(0);
            return Step::Stay;
        }
        if !self.details {
            return self.key(key);
        }
        let view = self.view.get();
        let page = view.rows.max(1);
        let top = self.top.get().min(view.max_top());
        let top = match key.code {
            KeyCode::PageUp => top.saturating_sub(page),
            KeyCode::PageDown => (top + page).min(view.max_top()),
            KeyCode::Home => 0,
            KeyCode::End => view.max_top(),
            _ => return self.key(key),
        };
        self.top.set(top);
        Step::Stay
    }

    fn key(&mut self, key: KeyEvent) -> Step<usize> {
        if prompt::is_interrupt_key(key) {
            return Step::Interrupt;
        }
        let len = self.len.max(1);
        match key.code {
            KeyCode::Up => self.focus = (self.focus + len - 1) % len,
            KeyCode::Down => self.focus = (self.focus + 1) % len,
            KeyCode::Enter if self.len > 0 => return Step::Done(self.focus),
            KeyCode::Esc => return Step::Abort,
            _ => {}
        }
        Step::Stay
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the keys and the details of the single-choice prompt.

    use super::*;

    fn press(state: &mut State, code: KeyCode, details: &Details) -> Step<usize> {
        state.on_key(KeyEvent::new(code, KeyModifiers::NONE), Some(details))
    }

    /// Return the plain text of `rows`.
    fn plain(rows: &[String]) -> Vec<String> {
        rows.iter()
            .map(|row| console::strip_ansi_codes(row).into_owned())
            .collect()
    }

    /// Test that a long line wraps into rows of the exact width, and that
    /// the rows keep all of the text. The screen counts one row for each
    /// line, so a row must not be wider than the screen.
    ///   1. Wrap a styled line of 25 characters into rows of 10 columns
    ///   2. Check that there are 3 rows of 10, 10 and 5 characters
    ///   3. Check that each row ends with a reset and the next row starts
    ///      with the style again
    #[test]
    fn long_styled_line_wraps_into_exact_rows() {
        let text = "abcdefghijklmnopqrstuvwxy";
        let line = format!("\x1b[31m{text}{RESET}");
        let rows = wrap_styled(&line, 10);
        assert_eq!(plain(&rows), ["abcdefghij", "klmnopqrst", "uvwxy"]);
        assert!(rows[0].starts_with("\x1b[31m") && rows[0].ends_with(RESET));
        assert!(rows[1].starts_with("\x1b[31m") && rows[1].ends_with(RESET));
        assert!(rows.iter().all(|row| measure_text_width(row) <= 10));
    }

    /// Test that wide characters and tabs count with their screen width, so
    /// that the row count stays exact.
    ///   1. Wrap a line with wide characters and a tab into rows of 8
    ///   2. Check the rows
    #[test]
    fn wide_characters_and_tabs_wrap_by_screen_width() {
        let rows = wrap_styled("漢字漢字漢\tx", 8);
        assert_eq!(plain(&rows), ["漢字漢字", "漢      ", "x"]);
        assert!(rows.iter().all(|row| measure_text_width(row) <= 8));
        assert_eq!(wrap_styled("", 8), [""]);
        assert_eq!(wrap_styled("漢a", 1), ["\u{fffd}", "a"]);
    }

    /// Test that the window of the details stays in the rows.
    ///   1. Ask for windows at the start, in the middle and after the end
    ///   2. Check that each window has the right rows
    #[test]
    fn details_window_stays_in_rows() {
        let view = View {
            rows: 10,
            total: 25,
            room: 80,
        };
        assert_eq!(view.window(0), 0..10);
        assert_eq!(view.window(7), 7..17);
        assert_eq!(view.window(100), 15..25);
        let small = View {
            rows: 10,
            total: 4,
            room: 80,
        };
        assert_eq!(small.window(3), 0..4);
    }

    /// Test that the details key shows and hides the details, in lower and
    /// upper case, and that other keys keep their normal action.
    ///   1. Press `s` and check that the details show
    ///   2. Press `S` and check that the details are hidden
    ///   3. Press Down and check that the focus moves
    #[test]
    fn details_key_shows_and_hides_details() {
        let details = Details {
            key: 's',
            show_hint: "show",
            hide_hint: "hide",
            lines: &[],
        };
        let mut state = State::new(0, 2);
        assert!(matches!(
            press(&mut state, KeyCode::Char('s'), &details),
            Step::Stay
        ));
        assert!(state.details);
        press(&mut state, KeyCode::Char('S'), &details);
        assert!(!state.details);
        press(&mut state, KeyCode::Down, &details);
        assert_eq!(state.focus, 1);
    }

    /// Return 50 detail lines. Some are longer than 20 columns.
    fn long_details() -> Vec<String> {
        (0..50)
            .map(|i| format!("line {i:02} {}", "x".repeat(i % 3 * 15)))
            .collect()
    }

    /// Return a question with two options and the details `lines`.
    fn question_with<'a>(
        choices: &'a [Choice<'a>],
        lines: &'a [String],
    ) -> (Choose<'a>, Details<'a>) {
        let question = Choose {
            title: "Title",
            notes: &["note"],
            choices,
            default: 0,
            report: false,
        };
        let details = Details {
            key: 's',
            show_hint: "show",
            hide_hint: "hide",
            lines,
        };
        (question, details)
    }

    /// Show the details in a terminal of `room` columns and `height` rows,
    /// and press PgDn until the end. Check that the frame always fits, and
    /// that each detail row shows one time or more.
    /// Returns:
    ///   The state at the end, and true if the focused option always showed.
    fn scroll_to_end(room: usize, height: usize) -> (State, bool) {
        let lines = long_details();
        let choices = [Choice { label: "Abort" }, Choice { label: "Approve" }];
        let (question, details) = question_with(&choices, &lines);
        let mut state = State::new(0, 2);
        press(&mut state, KeyCode::Char('s'), &details);
        let wanted: Vec<String> = lines.iter().flat_map(|l| wrap_styled(l, room)).collect();
        let mut seen = vec![false; wanted.len()];
        let mut focus_shown = true;
        let mut last_top = None;
        loop {
            let frame = question.frame(&state, room, height, Some(&details));
            if last_top == Some(state.top.get()) {
                break;
            }
            last_top = Some(state.top.get());
            assert!(frame.lines.len() <= height, "{height}: frame too tall");
            let shown = plain(&frame.lines);
            focus_shown &= shown.iter().any(|l| l.contains("Abort"));
            for i in state.view.get().window(state.top.get()) {
                assert!(shown.contains(&plain(&wanted[i..=i])[0]));
                seen[i] = true;
            }
            press(&mut state, KeyCode::PageDown, &details);
        }
        assert!(seen.iter().all(|s| *s), "{height}: not all rows shown");
        (state, focus_shown)
    }

    /// Test that the user can see each detail line in a small terminal. The
    /// user must see all of the changes before an approval.
    ///   1. Make 50 detail lines, some longer than the screen width
    ///   2. Show the details in a terminal of 12 rows and 20 columns
    ///   3. Press PgDn until the end and check that each row showed and
    ///      that the focused option always showed
    ///   4. Check that PgUp and Home go back
    #[test]
    fn each_detail_line_shows_when_scrolling() {
        let lines = long_details();
        let choices = [Choice { label: "Abort" }, Choice { label: "Approve" }];
        let (_, details) = question_with(&choices, &lines);
        let (mut state, focus_shown) = scroll_to_end(20, 12);
        assert!(focus_shown);
        press(&mut state, KeyCode::PageUp, &details);
        assert!(state.top.get() < state.view.get().max_top());
        press(&mut state, KeyCode::Home, &details);
        assert_eq!(state.top.get(), 0);
    }

    /// Test that each detail line shows in a terminal that is too small
    /// for the title, the notes and the options. The view must drop the
    /// other parts first, and keep the details.
    ///   1. For each height from 1 to 7 rows, show the details
    ///   2. Press PgDn until the end and check that the frame always fits
    ///      and that each row showed
    ///   3. Check that the focused option shows from 3 rows
    #[test]
    fn each_detail_line_shows_in_tiny_terminal() {
        for height in 1..=7 {
            let (_, focus_shown) = scroll_to_end(20, height);
            assert_eq!(focus_shown, height >= 3, "{height}");
        }
    }

    /// Test that a resize keeps the same detail line at the top. Else the
    /// window jumps to other text.
    ///   1. Show the details in 20 columns and scroll to the second row of
    ///      line 31
    ///   2. Make the frame for 40 columns
    ///   3. Check that line 31 is at the top
    #[test]
    fn resize_keeps_top_line() {
        let lines = long_details();
        let choices = [Choice { label: "Abort" }, Choice { label: "Approve" }];
        let (question, details) = question_with(&choices, &lines);
        let mut state = State::new(0, 2);
        press(&mut state, KeyCode::Char('s'), &details);
        question.frame(&state, 20, 12, Some(&details));
        let top: usize = lines[..31].iter().map(|l| wrap_styled(l, 20).len()).sum();
        state.top.set(top + 1);
        let frame = question.frame(&state, 40, 12, Some(&details));
        let shown = plain(&frame.lines);
        let first = shown.iter().position(|l| l.starts_with("line ")).unwrap();
        assert!(shown[first].starts_with("line 31"), "{}", shown[first]);
    }
}
