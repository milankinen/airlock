//! Text diff for the prompts.
//!
//! Shows the line changes between two texts as unified diff lines, with
//! some unchanged lines around each change. Removed lines are red and added
//! lines are green.

use similar::{ChangeTag, TextDiff};

use crate::cli::prompt::style::Styles;

/// Number of unchanged lines around each change in the diff.
const CONTEXT: usize = 3;

/// Return the line changes from `last` to `new` as unified diff lines.
/// Each line starts with `-` (removed, red), `+` (added, green) or a space
/// (unchanged, gray). A gray `…` line separates the change groups.
pub fn unified(last: &str, new: &str, styles: &Styles) -> Vec<String> {
    let diff = TextDiff::from_lines(last, new);
    let mut lines = Vec::new();
    for (i, group) in diff.grouped_ops(CONTEXT).iter().enumerate() {
        if i > 0 {
            lines.push(styles.dim.apply_to("  …").to_string());
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let line = change.value().trim_end_matches('\n');
                let (style, sign) = match change.tag() {
                    ChangeTag::Delete => (&styles.red, '-'),
                    ChangeTag::Insert => (&styles.green, '+'),
                    ChangeTag::Equal => (&styles.dim, ' '),
                };
                lines.push(style.apply_to(format!("{sign} {line}")).to_string());
            }
        }
    }
    lines
}
