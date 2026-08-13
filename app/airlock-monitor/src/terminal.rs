//! Which modifier key suspends mouse reporting in the host terminal.
//!
//! The TUI holds the terminal's mouse capture for the whole session, so a
//! plain drag never reaches the terminal's own text selection. Every
//! mainstream terminal has an escape hatch — hold a modifier and reporting
//! is bypassed for that drag — but *which* modifier differs, and a user
//! told the wrong one is worse off than a user told nothing.
//!
//! Detection is best-effort by nature. Over SSH, inside a multiplexer, or
//! under a terminal that advertises nothing, `TERM_PROGRAM` may be absent
//! or belong to some other program entirely — so the fallback matters more
//! than the table does.

use std::sync::OnceLock;

/// Near-universal default. Correct for xterm, GNOME Terminal, Konsole,
/// kitty, Alacritty, WezTerm, Windows Terminal, and tmux.
const SHIFT: &str = "Shift";
/// macOS terminals that bind selection-bypass to the Option key.
const OPTION: &str = "Option";
/// Terminal.app is the odd one out.
const FN: &str = "Fn";

/// Modifier to hold for text selection in the current terminal.
///
/// Cached — the environment cannot change mid-session.
pub fn select_modifier() -> &'static str {
    static CACHED: OnceLock<&'static str> = OnceLock::new();
    CACHED.get_or_init(|| {
        modifier_for(
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            std::env::var("LC_TERMINAL").ok().as_deref(),
            cfg!(target_os = "macos"),
        )
    })
}

/// Pure form of [`select_modifier`], split out so the table can be tested
/// without touching process environment — which is racy under a parallel
/// test runner and `unsafe` besides.
fn modifier_for(
    term_program: Option<&str>,
    lc_terminal: Option<&str>,
    macos: bool,
) -> &'static str {
    // iTerm2 sets LC_TERMINAL as well, and forwards it over SSH where
    // TERM_PROGRAM is typically lost — so check it either way round.
    if lc_terminal == Some("iTerm2") {
        return OPTION;
    }
    match term_program {
        Some("iTerm.app") => OPTION,
        Some("Apple_Terminal") => FN,
        // VS Code follows the platform's convention rather than its own.
        Some("vscode") if macos => OPTION,
        _ => SHIFT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iterm_uses_option_from_either_variable() {
        assert_eq!(modifier_for(Some("iTerm.app"), None, true), OPTION);
        // Over SSH, TERM_PROGRAM is usually gone but LC_TERMINAL survives.
        assert_eq!(modifier_for(None, Some("iTerm2"), false), OPTION);
    }

    #[test]
    fn terminal_app_uses_fn() {
        assert_eq!(modifier_for(Some("Apple_Terminal"), None, true), FN);
    }

    #[test]
    fn vscode_follows_the_platform() {
        assert_eq!(modifier_for(Some("vscode"), None, true), OPTION);
        assert_eq!(modifier_for(Some("vscode"), None, false), SHIFT);
    }

    /// The fallback carries more weight than the table: most sessions
    /// identify as nothing in particular, and Shift is right for almost
    /// every terminal that isn't listed above.
    #[test]
    fn anything_unrecognised_falls_back_to_shift() {
        assert_eq!(modifier_for(None, None, false), SHIFT);
        assert_eq!(modifier_for(None, None, true), SHIFT);
        assert_eq!(modifier_for(Some("WezTerm"), None, false), SHIFT);
        assert_eq!(modifier_for(Some("ghostty"), None, true), SHIFT);
        assert_eq!(modifier_for(Some(""), Some(""), true), SHIFT);
    }

    /// LC_TERMINAL wins over a conflicting TERM_PROGRAM: a multiplexer or
    /// SSH hop can leave a stale TERM_PROGRAM behind, but LC_TERMINAL is
    /// only set by the terminal that owns the session.
    #[test]
    fn lc_terminal_wins_over_a_stale_term_program() {
        assert_eq!(
            modifier_for(Some("Apple_Terminal"), Some("iTerm2"), true),
            OPTION
        );
    }
}
