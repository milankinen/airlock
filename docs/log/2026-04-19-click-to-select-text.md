# Click-to-select text in the sandbox tab

The monitor TUI captures mouse events so it can route clicks to its
interactive surfaces (tab bar, policy anchor, sub-tabs, network row
selection). The side effect: drag-to-select is lost while capture is
on, and the previous escape hatch was a manually-toggled `F12`
shortcut that nobody discovered without reading the manual.

The new flow:

- A left click anywhere in the Sandbox tab body drops mouse capture,
  so the host terminal handles the next drag natively. The footer
  switches to a yellow `Selection mode — Ctrl+C to copy, Esc to
  exit` hint.
- `Esc` or `Ctrl+C` while on the Sandbox tab re-enables capture.

The first click is eaten by the mode switch — we can't detect "user
is about to drag" before the terminal owns the mouse, so we pay for
the switch with the press that triggered it. The user starts the
actual drag on the next press. This is a known and accepted UX
trade-off; it's what "first click eats" means in every terminal
multiplexer that has a similar mode.

## Why only the Sandbox tab

The Monitor tab's body is full of interactive targets (network rows,
details close button, sub-tab labels, policy dropdown). Auto-dropping
capture there would make the tab unusable — every click would
deactivate the feature the click was meant to invoke. The Sandbox
tab body is a single scrollable terminal pane with no nested click
targets, so there's nothing to lose.

## Cmd+C on macOS

We don't try to detect `Cmd+C`. Every mainstream macOS terminal
(Terminal.app, iTerm2, Alacritty, WezTerm, kitty) intercepts
`Cmd+C` as the system copy shortcut and never forwards it to the
TUI. That's what makes the native "copy selection" flow work;
swallowing it in the app would break it. `Ctrl+C` covers both
platforms for the exit-selection-mode handler.

## F12 removed

With auto-enter on click and auto-exit on Esc/Ctrl+C, the manual
`F12` toggle is redundant. Removed the key handler, the mention in
`app.rs`'s `mouse_captured` docstring, and the `F12` row + "Mouse
capture" section from `docs/manual/src/monitor.md`. The manual now
documents the selection flow under "Selecting text" with a note
about the eaten first click.

## Implementation

- `handle_mouse` takes `mouse_captured: &mut bool`. The
  `MouseEventKind::Down(MouseButton::Left)` arm falls through past
  the existing Monitor-tab hit tests (tab bar, policy anchor,
  details close, sub-tabs — all of which now `return Ok(())` on
  match) and, if the active tab is Sandbox and capture is on,
  issues `DisableMouseCapture` + clears both the local flag and
  `app.mouse_captured`.
- `handle_key` gets an early branch after the global `F1`/`F2`
  shortcuts: on Sandbox tab with capture off, `Esc` or `Ctrl+C`
  issues `EnableMouseCapture` + sets both flags and returns. This
  sits before the per-tab key routing so `Ctrl+C` doesn't get
  forwarded to the guest PTY as SIGINT while we're in selection
  mode.
- `ui::build_status_line` prepends a yellow `Selection mode …`
  span when `!app.mouse_captured`, right-aligned in the footer
  next to the CPU/Memory/Network stats.
