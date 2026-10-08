//! Host terminal detection for text selection.
//!
//! Finds the modifier key that lets the user select text in the host terminal
//! when the sandboxed program uses the mouse.

use std::sync::OnceLock;

/// Near-universal default. Correct for xterm, GNOME Terminal, Konsole,
/// kitty, Alacritty, WezTerm, Windows Terminal, and tmux.
const SHIFT: &str = "Shift";
/// macOS terminals that bind selection-bypass to the Option key.
const OPTION: &str = "Option";
/// Terminal.app on macOS uses the Fn key.
const FN: &str = "Fn";

/// Modifier key to hold for text selection in the current terminal.
///
/// The TUI holds the terminal's mouse capture for the whole session. Thus a
/// plain drag never gets to the terminal's own text selection. All common
/// terminals bypass mouse reporting for a drag when the user holds a
/// modifier key. But the key is different in different terminals. A wrong
/// hint is worse than no hint.
///
/// The detection cannot be fully reliable. Over SSH, in a multiplexer, or
/// in a terminal that sets no variables, `TERM_PROGRAM` may be missing or
/// may belong to a different program. Thus the fallback is more important
/// than the table.
pub fn select_modifier() -> &'static str {
    // Cached, because the environment cannot change during the session.
    static CACHED: OnceLock<&'static str> = OnceLock::new();
    CACHED.get_or_init(|| {
        modifier_for(
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            std::env::var("LC_TERMINAL").ok().as_deref(),
            cfg!(target_os = "macos"),
        )
    })
}

/// Pure form of [`select_modifier`].
///
/// It is a separate function so that tests can check the table without
/// changes to the process environment. Such changes are racy when tests run
/// in parallel, and they are also `unsafe`.
fn modifier_for(
    term_program: Option<&str>,
    lc_terminal: Option<&str>,
    macos: bool,
) -> &'static str {
    // iTerm2 also sets LC_TERMINAL and sends it over SSH, where
    // TERM_PROGRAM is usually lost. Thus check LC_TERMINAL first.
    if lc_terminal == Some("iTerm2") {
        return OPTION;
    }
    match term_program {
        Some("iTerm.app") => OPTION,
        Some("Apple_Terminal") => FN,
        // VS Code uses the convention of the platform.
        Some("vscode") if macos => OPTION,
        _ => SHIFT,
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the selection modifier for each host terminal.

    use super::*;

    /// Test that the text selection modifier agrees with the host terminal and
    /// is Shift for unknown terminals. The hint must name the correct key.
    ///   1. Get the modifier for known and unknown terminals on macOS and on
    ///      other systems
    ///   2. Check the modifier of each case
    #[test]
    fn select_modifier_follows_terminal_and_falls_back_to_shift() {
        for (term_program, lc_terminal, macos, expected) in [
            (Some("iTerm.app"), None, true, OPTION),
            (None, Some("iTerm2"), false, OPTION),
            // LC_TERMINAL comes first, because SSH keeps it but often loses
            // TERM_PROGRAM.
            (Some("Apple_Terminal"), Some("iTerm2"), true, OPTION),
            (Some("Apple_Terminal"), None, true, FN),
            (Some("vscode"), None, true, OPTION),
            (Some("vscode"), None, false, SHIFT),
            (Some("WezTerm"), None, false, SHIFT),
            (Some("ghostty"), None, true, SHIFT),
            (Some(""), Some(""), true, SHIFT),
            (None, None, true, SHIFT),
            (None, None, false, SHIFT),
        ] {
            assert_eq!(
                modifier_for(term_program, lc_terminal, macos),
                expected,
                "{term_program:?} {lc_terminal:?} macos={macos}"
            );
        }
    }
}
