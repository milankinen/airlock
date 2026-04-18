# TUI monitor: fix HVP positioning, size PTY to body, move tab bar to bottom

## Motivation

Follow-up polish on `airlock start --monitor`. Three related issues:

1. **btop rendered as a scrambled single line inside the TUI.**
   The `vt100` crate handles `CSI r;c H` (CUP) but silently ignores the
   equivalent `CSI r;c f` (HVP) form. btop (and some other TUIs) use HVP
   exclusively, so every positioned write landed on whatever row the
   cursor happened to be on, collapsing the entire UI onto one line.

2. **Guest wrote past the visible body area.**
   In monitor mode the 2-row tab bar at the bottom is not part of the
   `vt100::Parser` grid, but the guest PTY was sized to the full host
   terminal. Anything drawn on the bottom two rows either fell off the
   grid or got folded back onto the last visible row.

3. **Tab bar + separate status bar ate two rows of vertical space** and
   the status-bar hotkey row duplicated the tab labels.

## HVP→CUP rewriter

The fix lives in a streaming `CsiRewriter` wrapped around
`vt100::Parser::process` inside `TuiTerminalSink::write`. It's a small
four-state machine (`Normal` → `Esc` → `Csi` with a `has_intro` flag
tracking whether a private-mode introducer `?`, `>`, `<`, or `=` was
seen). On the CSI final byte, `f` is rewritten to `H` only when no
private-mode introducer was present, so SGR, mode-set/reset, and similar
sequences pass through untouched. State persists across chunks — the
rewriter is called once per RPC output frame.

Upstreaming a patch to `vt100` would be preferable, but a local rewrite
is simpler, well-tested (6 unit tests including replay of real btop
sequences), and keeps us unblocked.

## Body-area PTY sizing

`cmd_start.rs` now advertises `(rows - TAB_BAR_HEIGHT) × cols` to the
guest when `--monitor` is active, matching what the `vt100::Parser`
grid actually covers. Resize events from crossterm already subtract the
tab bar before forwarding, so the two paths now agree.

## Bottom tab bar

The old layout had a 1-row tab bar at the top, the body in the middle,
and a 1-row status bar at the bottom that repeated the hotkeys.
Consolidated into a single 2-row block at the bottom (1 blank gap row
for the terminal's default bg, 1 row for the tabs). Hotkey labels are
embedded in the tab titles (`F1 Sandbox`, `F2 Network`) with the key
name in yellow on the tab's bg. Ctrl+Q is gone — the TUI already exits
when the sandbox process exits, so the shortcut was redundant.
