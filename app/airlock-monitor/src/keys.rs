//! Key bindings for the monitor TUI.
//!
//! Defines the actions that the user can bind to keys, and the default key for
//! each action. The user can change the bindings in the `[monitor.keys]`
//! config section. Also reads and shows keys in a text form.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// User-bindable action.
///
/// Actions do not depend on the context on purpose. For example, `Confirm`
/// means "confirm the item that the user sees": open the details from the
/// list view, or apply a policy from the dropdown. The dispatcher in
/// `lib.rs` sets the effect of each action from the current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    /// Go to the Sandbox tab from any state.
    SwitchSandbox,
    /// Go to the Monitor tab from any state.
    SwitchMonitor,
    /// Go back one step. In the list view, go to the Sandbox tab. In the
    /// details or the dropdown, close it.
    Back,
    /// Close the topmost modal (dropdown or details). No effect in the
    /// list view.
    Cancel,
    /// Open the details of the selected list entry, or apply the
    /// highlighted policy in the dropdown.
    Confirm,
    /// Send SIGHUP and SIGTERM to the sandbox process (Ctrl+D by default).
    KillSandbox,
    /// Move the selection up one row.
    SelectUp,
    /// Move the selection down one row.
    SelectDown,
    /// Move the selection up one page.
    SelectPageUp,
    /// Move the selection down one page.
    SelectPageDown,
    /// Select the newest entry.
    SelectNewest,
    /// Select the oldest entry.
    SelectOldest,
    /// Change between the Requests and Connections sub-tabs.
    ToggleSubTab,
    /// Open the Requests sub-tab.
    SelectRequests,
    /// Open the Connections sub-tab.
    SelectConnections,
    /// Open the network policy dropdown.
    OpenPolicy,
}

/// Map from a (KeyCode, KeyModifiers) tuple to an [`Action`].
///
/// Airlock builds it one time at startup from the user's config, or from
/// the defaults. The TUI reads it on each key event.
///
/// It also keeps a *primary* key for each action: the first key bound to
/// the action. The UI uses it to show shortcut hints (for example in tab
/// labels). Thus the hint is always the same key when an action has many
/// bindings.
#[derive(Debug, Clone, Default)]
pub struct KeyBindings {
    map: HashMap<(KeyCode, KeyModifiers), Action>,
    primary: HashMap<Action, (KeyCode, KeyModifiers)>,
}

impl KeyBindings {
    /// Action bound to the key event, or `None` if the key has no binding.
    pub fn lookup(&self, key: &KeyEvent) -> Option<Action> {
        self.map.get(&(key.code, key.modifiers)).copied()
    }

    /// Key to show for `action` in the UI: the first key bound to it.
    /// `None` if the action has no binding.
    pub fn primary(&self, action: Action) -> Option<(KeyCode, KeyModifiers)> {
        self.primary.get(&action).copied()
    }

    /// Bind keys to an action.
    /// Args:
    ///  - `action`: Action to bind
    ///  - `keys`: Key specs in the [`parse_key`] format. Invalid specs are
    ///    ignored.
    ///
    /// A later binding of the same key replaces the earlier one. The first
    /// valid key in `keys` becomes the primary key of the action. A later
    /// `bind()` for the same action *replaces* the primary key.
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
    /// Default bindings. They are the same as the bindings before the
    /// `[monitor.keys]` setting existed.
    pub fn defaults() -> Self {
        // Read from SPEC, so the defaults always agree with the action-name
        // lookup.
        let mut b = Self::default();
        for (_, action, keys) in SPEC {
            b.bind(*action, keys.iter().copied());
        }
        b
    }
}

/// Table of (kebab-case name, action, default keys) for all actions.
///
/// It is the single source of truth for:
///  * the default bindings
///  * the action lookup by name from the user's settings
///  * the action name in error messages.
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

/// Find an action by its kebab-case name.
/// Returns:
///   The action, or `None` for an unknown name. The caller (the settings
///   parser) reports the error.
pub fn action_for(name: &str) -> Option<Action> {
    SPEC.iter().find_map(|(n, a, _)| (*n == name).then_some(*a))
}

/// Parse a key spec string into a (KeyCode, KeyModifiers) tuple.
/// Args:
///  - `spec`: Key spec in the format `[<modifier>+]*<key>`.
///    Modifier names: `ctrl`, `alt`, `shift`, `super`.
///    Key names (case-insensitive): one ASCII char (`q`, `1`, `+`, `?`,
///    ...), `enter`, `esc`/`escape`, `tab`, `backspace`, `delete`, `space`,
///    `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`,
///    `f1`..`f12`.
///    Examples: `q`, `ctrl+d`, `shift+tab`, `f2`, `alt+enter`.
///
/// Returns:
///   The key code and modifiers, or an error message if the spec is not
///   valid.
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

    // Printable chars have no SHIFT modifier. Crossterm reports a plain `A`
    // for a shifted key, so `Shift+a` is confusing. Remove SHIFT from char
    // codes, so that user configs match the events from crossterm.
    let mods = if matches!(code, KeyCode::Char(_)) {
        mods - KeyModifiers::SHIFT
    } else {
        mods
    };

    Ok((code, mods))
}

/// Format a `(KeyCode, KeyModifiers)` pair as the label that the UI shows.
///
/// Approximately the inverse of [`parse_key`]. Modifier names are in title
/// case. Named keys use forms such as `F<n>` and `PageUp`. Single chars are
/// in uppercase, because that is easier to read as a label.
pub fn format_key((code, mods): (KeyCode, KeyModifiers)) -> String {
    let mut out = String::new();
    // Use the order that most users write: Ctrl, Alt, Shift, Super.
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
            // Always lowercase. See the SHIFT comment in parse_key.
            Some(KeyCode::Char(c.to_ascii_lowercase()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the key spec parser for user key bindings.

    use super::*;

    /// Test that the parser reads key names and modifiers in any letter case.
    /// User configs must match the key events from crossterm.
    ///   1. Parse char keys, named keys, function keys and modifier specs
    ///   2. Check the key code and modifiers of each spec
    #[test]
    fn parse_key_reads_names_modifiers_and_any_case() {
        for (spec, code, mods) in [
            ("q", KeyCode::Char('q'), KeyModifiers::NONE),
            ("Q", KeyCode::Char('q'), KeyModifiers::NONE),
            ("ctrl+d", KeyCode::Char('d'), KeyModifiers::CONTROL),
            // Crossterm reports no SHIFT for char keys, so the parser
            // removes it.
            ("shift+a", KeyCode::Char('a'), KeyModifiers::NONE),
            ("shift+tab", KeyCode::Tab, KeyModifiers::SHIFT),
            ("enter", KeyCode::Enter, KeyModifiers::NONE),
            ("f2", KeyCode::F(2), KeyModifiers::NONE),
            ("PageUp", KeyCode::PageUp, KeyModifiers::NONE),
        ] {
            assert_eq!(parse_key(spec), Ok((code, mods)), "{spec}");
        }
    }

    /// Test that a spec with an unknown modifier, an unknown key or a missing
    /// key is an error. The settings parser shows this error to the user.
    ///   1. Parse each bad spec
    ///   2. Check that each one gives an error
    #[test]
    fn parse_key_with_unknown_part_is_error() {
        for spec in ["hyper+x", "ctrl+nope", "f13", "", "ctrl+"] {
            assert!(parse_key(spec).is_err(), "{spec}");
        }
    }
}
