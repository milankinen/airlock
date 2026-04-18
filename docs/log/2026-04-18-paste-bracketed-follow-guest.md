# Safe multi-line paste — honor the guest shell's preference

## What

Safe multi-line paste in both `airlock start` modes, with the wrapping
decision driven by whatever the guest shell asked for:

- **Raw mode:** bracketed paste mode is no longer force-enabled on the
  host terminal. The guest shell's own `\e[?2004h` passes through and
  the host terminal reacts to it — same behavior the user would get in
  any other terminal.
- **TUI (`--monitor`) mode:** crossterm's `EnableBracketedPaste` is
  turned on so the host terminal wraps pastes and crossterm reports
  them as `Event::Paste(String)`. The TUI then watches guest PTY
  output for `\e[?2004h` / `\e[?2004l`, tracks whether the guest is
  currently in bracketed-paste mode, and wraps the forwarded paste in
  `\e[200~...\e[201~` only when it is.

## Why

Previous iteration (the commit before this) added bracketed-paste
wrapping unconditionally in both modes. That fixed the "multi-line
paste executes every line" problem for bash — but broke *single-line*
paste for BusyBox ash: the user pasted `echo "ok testing line 1"` and
saw only `esting line 1"` on the prompt. Trace logs confirmed all 36
bytes (markers + text) arrived at the guest PTY; bash would have
echoed 24 printable chars back, but ash echoed back just the 14-char
tail. BusyBox's line editor doesn't implement bracketed paste, so the
`\e[200~` prefix is parsed as a generic CSI key chord that silently
consumes surrounding bytes.

So we need to wrap for shells that support it (bash, zsh) and *not*
wrap for shells that don't (ash, dash, raw `cat`, etc.). The shell
itself is the only authority on whether it supports bracketed paste,
and it tells us by emitting `\e[?2004h` on its prompt. Following that
signal is both the most correct and the least configuration.

## How

### Raw mode

Removed the `\x1b[?2004h` write in `RawTerminalRuntime::enter_raw_mode`
and the matching disable in `TerminalGuard::drop`. In raw mode the
guest's stdout bytes pass through to the host terminal anyway — when
bash emits `\e[?2004h` the host terminal turns bracketed paste on and
pastes arrive as `\e[200~...\e[201~`, forwarded to the guest
unchanged. When ash is the shell, no enable sequence is emitted, the
host terminal stays off, pastes arrive raw. Each case matches what
the user would see in a normal (non-airlock) terminal running the
same shell.

### TUI mode

Two small additions:

- A `guest_bracketed_paste: bool` on `App`, scanned on every
  `TuiEvent::Output` via a byte-window search for `\x1b[?2004h` and
  `\x1b[?2004l`. The scan doesn't try to handle the sequence being
  split across chunks: guests re-emit it on each prompt redraw, so a
  single miss self-resolves on the next prompt.
- In the `Event::Paste` branch, wrap the text in `\e[200~...\e[201~`
  only when `app.guest_bracketed_paste` is true. Otherwise forward
  the text verbatim.

Also added the crossterm `EnableBracketedPaste` / `DisableBracketedPaste`
around the TUI session — required to make the host terminal wrap
pastes in the first place so crossterm can parse them into
`Event::Paste(String)` instead of fanning out as individual key
events (which would include the inter-line `Enter` and trigger
immediate execution just like the raw-mode bug).

## Trade-offs / what's deliberately not done

- **Ash still can't safely paste multi-line content.** That's a
  limitation of the shell's line editor, not our plumbing — no
  terminal can hold back newlines from a shell that treats every `\n`
  as Enter. Users who want safe multi-line paste should use bash or
  zsh inside the sandbox.
- **No split-sequence handling in the scanner.** The 8-byte toggle
  rarely lands across chunk boundaries, and the shell repaints its
  prompt (and so the toggle) often enough that the state converges
  quickly. Adding a small prefix-match buffer would be defensible but
  isn't worth the complexity today.

## Files

- `crates/airlock/src/runtime/raw_terminal.rs` — drop the forced
  bracketed-paste enable, update the doc comment.
- `crates/tui/src/app.rs` — `guest_bracketed_paste` field.
- `crates/tui/src/lib.rs` — `EnableBracketedPaste` /
  `DisableBracketedPaste` around the TUI session, `scan_bracketed_paste_mode`
  helper called on every `TuiEvent::Output`, conditional wrap in
  `Event::Paste` handler.
