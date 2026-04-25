# Monitor TUI settings

Make the monitor TUI's tunable bits (buffer caps, terminal scrollback,
key bindings) user-configurable via `~/.airlock/settings.toml` — the
same file that already holds `[vault]`. Defaults match what shipped
before, so existing configs are unaffected.

```toml
[monitor.buffers]
http       = 100
tcp        = 100
scrollback = 1000

[monitor.keys]
back         = "q"
cancel       = ["esc", "x"]
kill-sandbox = "ctrl+d"
# ... 16 actions total; only the ones you list are overridden
```

### Settings, not project config

Monitor knobs are personal preferences, not properties of the
sandbox. They don't belong in per-project `airlock.toml` (where the
choices would either get committed and inflicted on co-workers or
hide in `airlock.local.toml` and silently drift). Loading them from
`Settings` keeps them next to `[vault]` — the pattern was already in
place; we just plug into it.

`cmd_start::main` now takes `&Settings` (matching `cmd_secret::main`)
and threads `settings.monitor.*` into the runtime. The `MonitorSettings`
type lives in `airlock-cli/src/settings.rs`; the conversion to
`airlock_monitor::TuiSettings` happens at the CLI boundary so the
monitor crate stays free of config-format opinions.

### `[monitor.keys]` as a map, not a struct

The first take had `Keys` as a struct with one field per action
(`back: KeyList`, `cancel: KeyList`, etc.). smart-config's field-name
handling didn't cooperate well with kebab-case multi-segment names
like `select-page-up`, so the map shape (`BTreeMap<String, KeyList>`)
sidesteps that entirely — action names are *map keys*, not struct
field names.

The single source of truth for "what actions exist and what their
defaults are" lives in `airlock_monitor::keys::SPEC`:

```rust
pub const SPEC: &[(&str, Action, &[&str])] = &[
    ("switch-sandbox", Action::SwitchSandbox, &["f1"]),
    ("back",           Action::Back,          &["q"]),
    ("cancel",         Action::Cancel,        &["esc", "x"]),
    // ...
];
```

`KeyBindings::defaults()`, `action_for(name)`, and the CLI's
`into_bindings(user_map)` helper all read from this single table —
defaults can't drift from the name lookup, and adding an action means
editing exactly one place.

### `KeyList`: string OR list of strings

Each map value is a `KeyList(Vec<String>)` newtype with a custom
`serde::Deserialize` over an internal `untagged` enum, so users can
write either form:

```toml
back   = "q"            # single key
cancel = ["esc", "x"]   # list of keys
```

The `WellKnown` impl uses `Serde<{ STRING | ARRAY }>` so smart-config
admits both shapes.

### Custom key-string parser (no crossterm `serde`)

Crossterm's `serde` feature serialises `KeyCode` / `KeyModifiers` as
Rust-shaped enums (`{ Char = "q" }` plus a positional modifier array)
which is grim in TOML. ~60-line custom parser in
`airlock_monitor::keys::parse_key` accepts the natural `ctrl+d`,
`shift+tab`, `f2`, `alt+enter`, … plus modifier aliases (`option`/`meta`
for `alt`, `cmd`/`command` for `super`).

`shift+<letter>` is silently normalised to the bare lowercase letter:
crossterm reports shifted letters as plain uppercase chars without a
separate modifier flag, so `shift+a` would never match anything.

### Action enum and dispatcher refactor

`airlock_monitor::keys::Action` enumerates every bindable gesture
(16 variants). `KeyBindings` is a `HashMap<(KeyCode, KeyModifiers),
Action>` plus a per-action *primary* slot — the first key the action
was bound to, used by the UI to render shortcut hints deterministically
when an action has multiple bindings.

The TUI dispatcher (`handle_key` → `handle_monitor_action`):

1. Look up an `Action` for the key event.
2. Two global actions (`SwitchSandbox`, `SwitchMonitor`) intercept
   regardless of context.
3. On the Sandbox tab, every other key is passthrough to the PTY —
   the user can rebind any monitor action without breaking shell
   typing because we only intercept on the Monitor tab.
4. On the Monitor tab, sub-state (dropdown / details / list) decides
   what each action does.

Actions are intentionally context-agnostic: `Confirm` opens details
from the list view and applies a policy from the dropdown; `Back`
returns to the Sandbox tab from the list view but only closes the
modal from details/dropdown. Users bind keys to *intents*, not to a
context-by-context table.

### UI shows the actually-bound key

So that what the UI says matches what's bound:

- The bottom tab bar renders the bound primary key for
  `SwitchSandbox`/`SwitchMonitor` (`F1` / `F2` by default but
  whatever the user picked otherwise). `tab_header_rects` (mouse
  hit-test) shares the layout computation with `render_tab_bar` so
  click rectangles always match what was drawn.
- The `Requests` / `Connections` sub-tab labels still cyan-highlight
  their leading letter — but only when the user kept the default `r`
  / `c` bindings. Otherwise the label renders plain, with no
  misleading hint.

A new `keys::format_key` is the canonical reverse of `parse_key` and
produces display strings like `F1`, `Ctrl+D`, `PageUp`, `Esc`, `R`,
`Space`.

### Validation

Invalid key strings (unknown modifier, unknown key name, empty spec)
or unknown action names are collected into a single multi-line error
at config load time. The TUI refuses to launch instead of silently
dropping a binding — easier to spot a typo.

### `Settings::load` always goes through smart-config

Dropped `derive(Default)` on `Settings`. The "no settings file" path
in `load_from` now feeds an empty JSON object through smart-config so
per-field `default_t` annotations always apply — in particular the
key-binding defaults, which would otherwise need a duplicate
hand-written `Default` impl.

This eliminates a duplication risk: previously the keybinding
defaults would have existed in two places (the `default_t` annotations
and a hypothetical hand-written `Default`). Now there's one source of
truth (`SPEC`), and one path that produces a populated `Settings`.

### Out of scope

- Per-context bindings (e.g. "esc means X in dropdown but Y in list").
  The action layer makes this trivial to add later if needed; nothing
  in the current design rules it out.
- Rebinding the Sandbox tab keys. They're 100% passthrough by design
  — intercepting them would break terminal usage.
- Reload-on-config-change inside a running TUI session.
- Migration helper for users with an existing project-level
  `[monitor]` block — smart-config rejects the unknown field with a
  parse error pointing at the path, which is the right behaviour:
  the user moves it to settings and continues.
