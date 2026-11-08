//! Text layout of the dialog.
//!
//! Turns the values of a request into lines of fixed width. Long lines
//! break at the column limit, so that the user can read every character of
//! a command. Also keeps the scroll position of the text.

use crate::protocol::{DiffKind, Item};

/// Look of one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Style {
    /// Label of a value.
    Label,
    /// Plain text of a value.
    Text,
    /// Line that a change adds.
    Added,
    /// Line that a change removes.
    Removed,
    /// Line that a change keeps.
    Kept,
}

/// One line to draw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Line {
    /// Text of the line. It has no more characters than the column limit.
    pub text: String,
    /// Look of the line.
    pub style: Style,
}

/// Make the lines of all values.
/// Args:
///  - `items`: Values to show
///  - `cols`: Maximum number of characters in a line
pub(crate) fn build(items: &[Item], cols: usize) -> Vec<Line> {
    let mut out = Vec::new();
    for (n, item) in items.iter().enumerate() {
        if n > 0 {
            out.push(Line {
                text: String::new(),
                style: Style::Text,
            });
        }
        for text in wrap(&format!("{}:", item.label), cols) {
            out.push(Line {
                text,
                style: Style::Label,
            });
        }
        if let Some(diff) = &item.diff {
            for line in diff {
                let (mark, style) = match line.kind {
                    DiffKind::Add => ("+ ", Style::Added),
                    DiffKind::Remove => ("- ", Style::Removed),
                    DiffKind::Keep => ("  ", Style::Kept),
                };
                for text in wrap(&format!("{mark}{}", line.text), cols) {
                    out.push(Line { text, style });
                }
            }
        } else {
            for text in wrap(&item.value, cols) {
                out.push(Line {
                    text,
                    style: Style::Text,
                });
            }
        }
    }
    out
}

/// Break text into lines of at most `cols` characters. A line break of the
/// text stays. Tabs become spaces, and other control characters become
/// `?`, so that the text cannot move the drawing.
pub(crate) fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut out = Vec::new();
    for source in text.split('\n') {
        let mut line = String::new();
        let mut count = 0;
        for c in source.chars() {
            let cells: &[char] = match c {
                '\t' => &[' ', ' ', ' ', ' '],
                c if c.is_control() => &['?'],
                _ => &[c],
            };
            for &c in cells {
                if count == cols {
                    out.push(std::mem::take(&mut line));
                    count = 0;
                }
                line.push(c);
                count += 1;
            }
        }
        out.push(line);
    }
    out
}

/// Scroll position in a list of lines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Scroll {
    /// Index of the first line that shows.
    pub offset: usize,
    /// Number of lines that fit in the view.
    pub visible: usize,
    /// Number of lines in all.
    pub total: usize,
}

impl Scroll {
    /// Last valid offset.
    fn max_offset(&self) -> usize {
        self.total.saturating_sub(self.visible)
    }

    /// Move by `delta` lines. The offset stays in the valid range.
    pub fn by(&mut self, delta: isize) {
        self.offset = self
            .offset
            .saturating_add_signed(delta)
            .min(self.max_offset());
    }

    /// Set the size of the view and the list, and keep the offset valid.
    pub fn set_size(&mut self, visible: usize, total: usize) {
        self.visible = visible;
        self.total = total;
        self.offset = self.offset.min(self.max_offset());
    }

    /// Move to the first line.
    pub fn home(&mut self) {
        self.offset = 0;
    }

    /// Move to the last position.
    pub fn end(&mut self) {
        self.offset = self.max_offset();
    }

    /// True if the list does not fit in the view.
    pub fn scrolls(&self) -> bool {
        self.total > self.visible
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DiffLine;

    /// Test that a long command shows in full, over more lines.
    ///   1. Wrap a command that is longer than the column limit
    ///   2. Check that no line is too long and no character is lost
    #[test]
    fn long_line_breaks_without_losing_characters() {
        let text = "curl https://example.com/".repeat(10);
        let lines = wrap(&text, 20);
        assert!(lines.iter().all(|l| l.chars().count() <= 20));
        assert_eq!(lines.concat(), text);
    }

    /// Test that control characters cannot move the drawing.
    ///   1. Wrap text with tab, escape and carriage return characters
    ///   2. Check that the lines have only spaces and `?` in their place
    #[test]
    fn control_characters_are_replaced() {
        let lines = wrap("a\tb\x1b[31m\rc\nd", 80);
        assert_eq!(lines, vec!["a    b?[31m?c".to_string(), "d".to_string()]);
    }

    /// Test the lines of a value and of a change.
    ///   1. Build the lines of a plain value and of a diff value
    ///   2. Check labels, marks and styles
    #[test]
    fn items_become_styled_lines() {
        let items = vec![
            Item {
                label: "Command".into(),
                value: "ls\npwd".into(),
                diff: None,
            },
            Item {
                label: "Changes".into(),
                value: String::new(),
                diff: Some(vec![
                    DiffLine {
                        kind: DiffKind::Remove,
                        text: "allow = []".into(),
                    },
                    DiffLine {
                        kind: DiffKind::Add,
                        text: "allow = [\"a.com\"]".into(),
                    },
                ]),
            },
        ];
        let lines = build(&items, 40);
        let shown: Vec<_> = lines.iter().map(|l| (l.text.as_str(), l.style)).collect();
        assert_eq!(
            shown,
            vec![
                ("Command:", Style::Label),
                ("ls", Style::Text),
                ("pwd", Style::Text),
                ("", Style::Text),
                ("Changes:", Style::Label),
                ("- allow = []", Style::Removed),
                ("+ allow = [\"a.com\"]", Style::Added),
            ]
        );
    }

    /// Test that the scroll offset stays in range.
    ///   1. Scroll past both ends of a list
    ///   2. Shrink the list and check that the offset follows
    #[test]
    fn scroll_offset_stays_in_range() {
        let mut s = Scroll::default();
        s.set_size(10, 25);
        s.by(100);
        assert_eq!(s.offset, 15);
        s.by(-100);
        assert_eq!(s.offset, 0);
        s.end();
        s.set_size(10, 12);
        assert_eq!(s.offset, 2);
        assert!(s.scrolls());
    }
}
