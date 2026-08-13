# Always pass the mouse through; replace selection mode with a hint

## Context

`mouse_passthrough` shipped with `none` as the default and `all` as an
opt-in. In practice `all` is the behaviour people want — the wheel and
clicks belong to whatever mouse-aware program is running — and the knob
mostly exists to protect a fallback that is itself the problem.

That fallback is **selection mode**: a click drops `EnableMouseCapture`
so the host terminal can drag-select, restored by Esc/Ctrl+C. Under
`all` its only entrance disappears anyway (the click goes to the guest),
so the mode is half-dead already. Meanwhile every mainstream terminal
already has a bypass for exactly this — hold a modifier and mouse
reporting is suspended for the drag — which works regardless of what the
TUI is doing.

So: drop the setting, always forward, delete selection mode, and instead
**teach** the terminal's own bypass at the moment the user reaches for
it. Clicking shows `Hold <key> to select text` for two seconds, where
`<key>` is the modifier that actually works in the detected terminal.

One consequence worth stating plainly: selection mode is also what lets
you copy request/response headers out of the **Monitor tab's details
view** (`lib.rs:766`, documented at `usage/monitor.md:203-206`). That has
nothing to do with the sandbox or passthrough. It changes too — the same
modifier covers it, and the same hint fires there, but it is a real
behaviour change beyond the Sandbox tab.

## Design decisions

Three small calls, made here so they are not silently decided in code:

- **The hint fires on any left-button-down**, wherever it lands. Not on
  the wheel, not on right/middle, and not on drag or release — those are
  not the gesture someone makes when they mean to select. No rect test:
  a click is a click, and restricting it by area buys precision nobody
  asked for at the cost of a condition that can be wrong.
- **The `Mouse → sandbox` indicator goes away entirely.** It existed to
  explain a setting that no longer exists; with forwarding unconditional
  there is no state for it to disclose. The status slot therefore holds
  the transient hint or nothing, and `build_status_line`/`render_tab_bar`
  lose the `guest_owns_mouse` argument the mouse branch threaded in.
- **Detection is a pure function of `(TERM_PROGRAM, LC_TERMINAL, os)`**
  with a thin env-reading wrapper. Tests then need no `set_var`, which is
  both racy under a parallel test runner and `unsafe` in edition 2024.

## Approach

### 1. Delete the setting

- `app/airlock-cli/src/settings.rs` — drop `MonitorMousePassthrough`, the
  `monitor.mouse_passthrough` field, its `WellKnown`/`From` impls, and the
  three tests at ~line 267.
- `app/airlock-monitor/src/mouse.rs` — drop the `MousePassthrough` enum;
  keep the encoder, which is the valuable part of that module.
- `app/airlock-monitor/src/settings.rs` — drop the field and its default.
- `app/airlock-monitor/src/lib.rs:26` — drop the re-export.
- `app/airlock-cli/src/cli/cmd_start.rs:129` — drop the assignment.
- `guest_owns_mouse` (`lib.rs:661`) loses its settings clause and becomes
  active tab + guest mouse mode. Keep the function and its doc comment
  about *why* the guest's own mode is consulted — that reasoning is what
  makes always-on forwarding safe at a shell prompt.

⚠️ `network.passthrough` in `airlock-cli` is an unrelated TLS feature.
Do not touch it.

### 2. Delete selection mode

- `app/airlock-monitor/src/app.rs` — remove `mouse_captured`; add
  `select_hint_at: Option<Instant>`.
- `app/airlock-monitor/src/lib.rs` — remove the local `mouse_captured`
  and every `&mut bool` threaded through `handle_event`/`handle_key`/
  `handle_mouse`; remove `exits_selection_mode` (`lib.rs:469`) and its
  call site; remove both `DisableMouseCapture` click branches
  (`lib.rs:766-771`, `lib.rs:778-781`) and the Esc/Ctrl+C restore
  branches (`lib.rs:503-506`, `lib.rs:525-528`).

Keep the startup `EnableMouseCapture` and teardown `DisableMouseCapture`
(`lib.rs:245`, `lib.rs:269`) — capture is now unconditional for the whole
session.

Removing the Esc/Ctrl+C interception means both keys now reach the guest
in every state. That is the intended outcome (Ctrl+C should interrupt the
sandboxed program), but it is the one change most likely to surprise, so
it belongs in the log entry.

### 3. Terminal detection — new `app/airlock-monitor/src/terminal.rs`

```rust
/// Modifier that suspends mouse reporting for a drag, by terminal.
pub fn select_modifier() -> &'static str;              // env wrapper, cached
fn modifier_for(term_program: Option<&str>,
                lc_terminal: Option<&str>,
                macos: bool) -> &'static str;          // pure, tested
```

| Detected | Modifier |
|---|---|
| `TERM_PROGRAM=iTerm.app`, or `LC_TERMINAL=iTerm2` | `Option` |
| `TERM_PROGRAM=Apple_Terminal` | `Fn` |
| `TERM_PROGRAM=vscode` | `Option` on macOS, else `Shift` |
| anything else, or nothing set | `Shift` |

The first three rows are lifted from Claude Code's own per-terminal
table, which is the only corroborated source we have; everything else
defaults to `Shift`, which is the near-universal convention. Detection is
best-effort by nature — over SSH or inside a multiplexer `TERM_PROGRAM`
may be absent or belong to a different program — so the default matters
more than the table. Cache in a `OnceLock`; the environment cannot change
mid-session.

### 4. The hint

- Record `app.select_hint_at = Some(Instant::now())` at the top of
  `handle_mouse` on any left-button-down, **before** the passthrough early
  return — otherwise the common case (click reaches the guest) shows
  nothing.
- `ui::build_status_line` renders `Hold {modifier} to select text` while
  `select_hint_at` is within 2s, and nothing otherwise. The
  `guest_owns_mouse` argument added by the mouse branch is dropped from
  both `build_status_line` and `render_tab_bar`.
- No new scheduling: the loop already redraws every 16ms via
  `rx.recv_timeout` (`lib.rs:329`), so the hint expires on its own.

### 5. Config back-compat — verification gate, decide on evidence

`parse_settings` (`settings.rs:167`) carries a comment claiming unknown
fields become parse errors. If true, deleting the key **breaks every
existing user's `settings.toml` on upgrade** with a hard startup failure.
I could not confirm it read-only: no test asserts it and smart-config's
source shows no obvious unknown-key rejection, so the comment may simply
be aspirational.

**First implementation step, before deleting anything:** add a temporary
test that loads a `settings.toml` containing a key no longer in the
schema, and see whether it errors.

- Ignored → delete the field outright, no shim.
- Errors → keep `mouse_passthrough` as an accepted, deprecated, ignored
  field so existing configs still start, and note the deprecation in the
  manual. Deleting it regardless would be a gratuitous breaking change
  for a setting we are removing precisely because nobody should need it.

### 6. Docs

- `docs/manual/src/usage/monitor.md` — delete the `mouse_passthrough`
  section (~202-212); rewrite "Selecting text" (~194-206) around the
  modifier and the hint; **fix line 262**, which currently lists iTerm2
  under `Shift` and is wrong — that error is what prompted this work.
- Dev log `docs/log/2026-08-11-mouse-always-passthrough.md`: why the knob
  went, why selection mode went with it, the Monitor-tab consequence, the
  Esc/Ctrl+C change, and that the modifier table is best-effort.

## Verification

`mise format`, `mise run lint`, `mise x -- cargo test --workspace`.

- **Unit** — `modifier_for` across the table plus unknown/absent input;
  hint visibility as a pure function of two instants (visible at 0s and
  1.9s, gone at 2.1s); `guest_owns_mouse` no longer consulting settings.
- **Removed** — the three `mouse_passthrough` settings tests.
- **bats** — `tests/vm/*.bats` cannot run here (no KVM); `tests/cli/`
  can, via `mise x -- bats tests/cli/` after building the release binary.
  Note `show.bats` 27/28 already fail on main for unrelated reasons.

Manual check, since the payoff is interactive and untestable in CI: run
`airlock start --monitor`, click in the sandbox body, confirm the hint
appears for ~2s and that holding the named modifier really does select in
that terminal. Worth doing in at least iTerm2 (`Option`) and one
`Shift` terminal, because a wrong modifier in the table is worse than no
hint at all — it teaches the user something false.
