//! Key-to-action lookup for the monitor TUI.
//!
//! Each user gesture is modelled as an [`Action`]. The CLI builds a
//! [`KeyBindings`] map (action → set of keys) from the user's
//! `[monitor.keys]` config and hands it to the TUI; the dispatcher then
//! resolves a [`crossterm::event::KeyEvent`] into an `Action` (or `None`
//! for keys that should pass through to the sandbox PTY).
//!
//! Actions are intentionally context-agnostic: `Confirm` means
//! "confirm whatever the user is looking at" — open details from the
//! list view, apply a policy from the dropdown. The dispatcher in
//! `lib.rs` decides what each action does given the current state.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// User-bindable actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    /// Force-switch to the Sandbox tab.
    SwitchSandbox,
    /// Force-switch to the Monitor tab.
    SwitchMonitor,
    /// Step back: in list view, return to Sandbox tab; in details
    /// or dropdown, close the modal.
    Back,
    /// Dismiss the topmost modal (dropdown / details). No-op in list view.
    Cancel,
    /// Confirm: open details on the selected list entry, or apply the
    /// highlighted policy in the dropdown.
    Confirm,
    /// Send SIGHUP+SIGTERM to the sandbox process (Ctrl+D by default).
    KillSandbox,
    SelectUp,
    SelectDown,
    SelectPageUp,
    SelectPageDown,
    SelectNewest,
    SelectOldest,
    /// Toggle between Requests and Connections sub-tabs.
    ToggleSubTab,
    SelectRequests,
    SelectConnections,
    OpenPolicy,
}

/// Map from a (KeyCode, KeyModifiers) tuple to an Action. Built once at
/// startup from the user's config (or from defaults) and consulted on
/// every key event.
///
/// Also tracks a per-action *primary* key — the first key the action
/// was bound to. Used by the UI to render shortcut hints (tab labels,
/// etc.) deterministically when an action has multiple bindings.
#[derive(Debug, Clone, Default)]
pub struct KeyBindings {
    map: HashMap<(KeyCode, KeyModifiers), Action>,
    primary: HashMap<Action, (KeyCode, KeyModifiers)>,
}

impl KeyBindings {
    /// Look up the action bound to a specific key event, if any.
    pub fn lookup(&self, key: &KeyEvent) -> Option<Action> {
        self.map.get(&(key.code, key.modifiers)).copied()
    }

    /// The "display" key for an action — the first key it was bound
    /// to. `None` when the action has no binding.
    pub fn primary(&self, action: Action) -> Option<(KeyCode, KeyModifiers)> {
        self.primary.get(&action).copied()
    }

    /// Bind every parsed key in `keys` to `action`. Subsequent calls
    /// for the same key overwrite earlier bindings — last write wins.
    /// The first parsed key in the iterator becomes the action's
    /// primary key (used for UI display); a later `bind()` for the
    /// same action *replaces* the primary.
    pub fn bind<I, S>(&mut self, action: Action, keys: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut first = true;
        for k in keys {
            if let Ok((code, mods)) = parse_key(k.as_ref()) {
                self.map.insert((code, mods), action);
                if first {
                    self.primary.insert(action, (code, mods));
                    first = false;
                }
            }
        }
    }
}

impl KeyBindings {
    /// Default bindings — mirrors what the TUI shipped with before the
    /// `[monitor.keys]` setting existed. Reads straight from [`SPEC`]
    /// so defaults can't drift from the action-name lookup.
    pub fn defaults() -> Self {
        let mut b = Self::default();
        for (_, action, keys) in SPEC {
            b.bind(*action, keys.iter().copied());
        }
        b
    }
}

/// Canonical (kebab-case-name, action, default-keys) table. The single
/// source of truth used to: build the default bindings, look up an
/// action from a string in the user's settings, and report a human
/// name for an action in error messages.
pub const SPEC: &[(&str, Action, &[&str])] = &[
    ("switch-sandbox", Action::SwitchSandbox, &["f1"]),
    ("switch-monitor", Action::SwitchMonitor, &["f2"]),
    ("back", Action::Back, &["q"]),
    ("cancel", Action::Cancel, &["esc", "x"]),
    ("confirm", Action::Confirm, &["enter"]),
    ("kill-sandbox", Action::KillSandbox, &["ctrl+d"]),
    ("select-up", Action::SelectUp, &["up"]),
    ("select-down", Action::SelectDown, &["down"]),
    ("select-page-up", Action::SelectPageUp, &["pageup"]),
    ("select-page-down", Action::SelectPageDown, &["pagedown"]),
    ("select-newest", Action::SelectNewest, &["home"]),
    ("select-oldest", Action::SelectOldest, &["end"]),
    (
        "toggle-sub-tab",
        Action::ToggleSubTab,
        &["tab", "left", "right"],
    ),
    ("select-requests", Action::SelectRequests, &["r"]),
    ("select-connections", Action::SelectConnections, &["c"]),
    ("open-policy", Action::OpenPolicy, &["p"]),
];

/// Look up an action by its kebab-case name. Returns `None` for
/// unknown names; callers (the settings parser) report the typo.
pub fn action_for(name: &str) -> Option<Action> {
    SPEC.iter().find_map(|(n, a, _)| (*n == name).then_some(*a))
}

/// Parse a key spec string into a (KeyCode, KeyModifiers) tuple.
///
/// Format: `[<modifier>+]*<key>`. Modifier names: `ctrl`, `alt`,
/// `shift`, `super`. Key names (case-insensitive): single ASCII char
/// (`q`, `1`, `+`, `?`, ...), `enter`, `esc`/`escape`, `tab`,
/// `backspace`, `delete`, `space`, `up`, `down`, `left`, `right`,
/// `home`, `end`, `pageup`, `pagedown`, `f1`..`f12`.
///
/// Examples: `q`, `ctrl+d`, `shift+tab`, `f2`, `alt+enter`.
pub fn parse_key(spec: &str) -> Result<(KeyCode, KeyModifiers), String> {
    let parts: Vec<&str> = spec.split('+').map(str::trim).collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Err(format!("empty key spec: `{spec}`"));
    }
    let (key_part, mod_parts) = parts.split_last().expect("non-empty");

    let mut mods = KeyModifiers::NONE;
    for m in mod_parts {
        let bit = match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" | "option" | "meta" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            "super" | "cmd" | "command" => KeyModifiers::SUPER,
            other => return Err(format!("unknown modifier `{other}` in key spec `{spec}`")),
        };
        mods |= bit;
    }

    let code = parse_code(key_part)
        .ok_or_else(|| format!("unknown key `{key_part}` in key spec `{spec}`"))?;

    // Single printable chars carry no shift modifier — `Shift+a` would be
    // confusing because crossterm reports plain `A` for shifted keys.
    // Strip SHIFT for char codes so user configs match what crossterm emits.
    let mods = if matches!(code, KeyCode::Char(_)) {
        mods - KeyModifiers::SHIFT
    } else {
        mods
    };

    Ok((code, mods))
}

/// Render a `(KeyCode, KeyModifiers)` pair as the canonical display
/// string the UI shows to users. Roughly the inverse of [`parse_key`],
/// using title-case modifier names and `F<n>` / `PageUp` etc. for named
/// keys. Single chars come back uppercase to read naturally as a label.
pub fn format_key((code, mods): (KeyCode, KeyModifiers)) -> String {
    let mut out = String::new();
    // Order matches what most users write: Ctrl, Alt, Shift, Super.
    if mods.contains(KeyModifiers::CONTROL) {
        out.push_str("Ctrl+");
    }
    if mods.contains(KeyModifiers::ALT) {
        out.push_str("Alt+");
    }
    if mods.contains(KeyModifiers::SHIFT) {
        out.push_str("Shift+");
    }
    if mods.contains(KeyModifiers::SUPER) {
        out.push_str("Super+");
    }
    let key = match code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    };
    out.push_str(&key);
    out
}

fn parse_code(s: &str) -> Option<KeyCode> {
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
        "enter" | "return" => Some(KeyCode::Enter),
        "esc" | "escape" => Some(KeyCode::Esc),
        "tab" => Some(KeyCode::Tab),
        "backspace" | "bs" => Some(KeyCode::Backspace),
        "delete" | "del" => Some(KeyCode::Delete),
        "space" => Some(KeyCode::Char(' ')),
        "up" => Some(KeyCode::Up),
        "down" => Some(KeyCode::Down),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        "pageup" | "pgup" => Some(KeyCode::PageUp),
        "pagedown" | "pgdn" | "pgdown" => Some(KeyCode::PageDown),
        s if s.starts_with('f') && s.len() <= 3 => {
            let n: u8 = s[1..].parse().ok()?;
            (1..=12).contains(&n).then_some(KeyCode::F(n))
        }
        s if s.chars().count() == 1 => {
            let c = s.chars().next().expect("len 1");
            // Always lowercase: see comment in parse_key about SHIFT.
            Some(KeyCode::Char(c.to_ascii_lowercase()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_char() {
        assert_eq!(parse_key("q"), Ok((KeyCode::Char('q'), KeyModifiers::NONE)));
        assert_eq!(parse_key("Q"), Ok((KeyCode::Char('q'), KeyModifiers::NONE)));
    }

    #[test]
    fn parse_ctrl_d() {
        assert_eq!(
            parse_key("ctrl+d"),
            Ok((KeyCode::Char('d'), KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn parse_named_keys() {
        assert_eq!(parse_key("enter"), Ok((KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(parse_key("f2"), Ok((KeyCode::F(2), KeyModifiers::NONE)));
        assert_eq!(
            parse_key("PageUp"),
            Ok((KeyCode::PageUp, KeyModifiers::NONE))
        );
    }

    #[test]
    fn parse_shift_tab() {
        assert_eq!(
            parse_key("shift+tab"),
            Ok((KeyCode::Tab, KeyModifiers::SHIFT))
        );
    }

    #[test]
    fn shift_stripped_for_chars() {
        // Crossterm reports shifted letters as plain uppercase chars
        // with no SHIFT modifier — make sure binding `shift+a` matches.
        let (code, mods) = parse_key("shift+a").unwrap();
        assert_eq!(code, KeyCode::Char('a'));
        assert_eq!(mods, KeyModifiers::NONE);
    }

    #[test]
    fn unknown_modifier_errors() {
        assert!(parse_key("hyper+x").is_err());
    }

    #[test]
    fn unknown_key_errors() {
        assert!(parse_key("ctrl+nope").is_err());
    }

    #[test]
    fn bindings_lookup() {
        let b = KeyBindings::defaults();
        let evt = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(b.lookup(&evt), Some(Action::Back));
        let evt = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(b.lookup(&evt), Some(Action::KillSandbox));
    }
}
