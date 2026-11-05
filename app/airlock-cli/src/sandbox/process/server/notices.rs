//! Notice log of the sandbox.
//!
//! Keeps the messages of the sandbox for the user during one run, for
//! example a warning of the start or a refused browser request. All parts
//! of the sandbox process add their notices to one log. The log only
//! grows: it never removes or changes a notice. Each notice has a number.
//! Thus each client reads the full log and finds the new notices by their
//! numbers.
//!
//! A notice is one line of plain text. The log removes colors and control
//! characters, and cuts a long text. Code in the sandbox can cause notices,
//! for example with a browser request. Thus a notice must not control the
//! terminal of the user.
//!
//! The log keeps a text only one time, so that repeated notices do not
//! fill it. The log has a size limit. After the limit, one last notice
//! tells the user that the sandbox does not keep more notices. The sandbox
//! log keeps the later notices.

use std::sync::{Arc, Mutex, PoisonError};

use crate::messages;
use crate::sandbox::process::Notice;

/// Maximum number of notices in the log, without the last notice about the
/// limit.
const MAX_NOTICES: usize = 32;

/// Maximum number of characters of one notice text, without the cut mark.
const MAX_TEXT: usize = 1000;

/// Mark at the end of a text that the log cut.
const CUT_MARK: &str = " [...]";

/// Notice log of one sandbox run. Cheap to clone. All clones share the log.
// Thread safe: the warnings of the process can come from any thread.
#[derive(Clone, Default)]
pub struct NoticeLog(Arc<Mutex<Vec<Notice>>>);

impl NoticeLog {
    /// Make an empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the notice `text` at the end of the log as plain text (see
    /// [`plain_text`]). Does nothing if the log has the same text. After
    /// the limit, the log gets one notice about the limit, and then nothing
    /// more. The sandbox log gets each notice that the log does not keep.
    pub fn push(&self, text: String) {
        let text = plain_text(text);
        let mut items = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if items.iter().any(|n| n.text == text) {
            tracing::debug!("notice again: {text}");
            return;
        }
        let text = match items.len() {
            n if n < MAX_NOTICES => text,
            n => {
                tracing::info!("notice not kept: {text}");
                if n > MAX_NOTICES {
                    return;
                }
                messages::sandbox_process::NOTICE_LIMIT.to_string()
            }
        };
        // The numbers start at 1. Thus 0 means "no notice" for a client.
        let seqno = items.len() as u64 + 1;
        items.push(Notice { seqno, text });
    }

    /// Get all notices of the log, oldest first.
    pub fn all(&self) -> Vec<Notice> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Make `text` safe for the terminal of a client: remove the colors and
/// the other escape sequences, change line breaks and tabs to spaces,
/// remove the other control characters and the text direction marks, and
/// cut a long text with a mark.
fn plain_text(text: String) -> String {
    // Most texts are plain already. Keep them as they are.
    let unsafe_char = |c: char| c.is_control() || is_direction_mark(c);
    if !text.contains(unsafe_char) && text.chars().count() <= MAX_TEXT {
        return text;
    }
    let stripped = console::strip_ansi_codes(&text);
    let mut out = String::new();
    for (i, c) in stripped
        .chars()
        .filter(|c| !is_direction_mark(*c))
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control())
        .enumerate()
    {
        if i == MAX_TEXT {
            out.push_str(CUT_MARK);
            break;
        }
        out.push(c);
    }
    out
}

/// Check if `c` changes the text direction. Such a mark can make a text
/// show other than it is.
fn is_direction_mark(c: char) -> bool {
    matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that a full log keeps its notices and their numbers, and adds
    /// only one notice about the limit. A reader must never see a changed
    /// or a removed notice. A repeated text must not use a place in the
    /// log, because code in the sandbox can repeat a refused request.
    ///   1. Add more different notices than the limit, each one two times
    ///   2. Check that the first notices and their numbers did not change
    ///   3. Check that the last notice tells about the limit, one time
    #[test]
    fn full_log_keeps_old_notices_and_adds_one_limit_notice() {
        let log = NoticeLog::new();
        for i in 0..MAX_NOTICES + 5 {
            log.push(format!("n{i}"));
            log.push(format!("n{i}"));
        }
        let all = log.all();
        assert_eq!(all.len(), MAX_NOTICES + 1);
        for (i, notice) in all.iter().take(MAX_NOTICES).enumerate() {
            assert_eq!(notice.seqno, i as u64 + 1);
            assert_eq!(notice.text, format!("n{i}"));
        }
        let last = all.last().unwrap();
        assert_eq!(last.seqno, MAX_NOTICES as u64 + 1);
        assert_eq!(last.text, messages::sandbox_process::NOTICE_LIMIT);
    }

    /// Test that a notice is one line of plain text with a length limit.
    /// Code in the sandbox can put any text into a notice, and the client
    /// prints it on the terminal of the user.
    ///   1. Add a text with colors, a carriage return, a bell, a line break,
    ///      a tab, a lone escape character and a direction mark
    ///   2. Check that only the plain text with spaces is left
    ///   3. Add a text over the length limit and check the cut and the mark
    #[test]
    fn notice_text_is_plain_line_with_length_limit() {
        let log = NoticeLog::new();
        log.push("\x1b[31mred\x1b[0m\rover\x07bell\nnext\ttab\x1b lone\u{202e}rtl".into());
        log.push("x".repeat(MAX_TEXT + 10));
        let all = log.all();
        assert_eq!(all[0].text, "red overbell next tab lonertl");
        assert_eq!(all[1].text, format!("{}{CUT_MARK}", "x".repeat(MAX_TEXT)));
    }
}
