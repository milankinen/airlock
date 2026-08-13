# Always pass the mouse through; replace selection mode with a hint

## Motivation

`mouse_passthrough` shipped with `none` as the default and `all` as an
opt-in. `all` is what people actually want — the wheel and clicks belong
to whatever mouse-aware program is running — and the knob mostly existed
to protect the fallback that turned out to be the problem.

That fallback was **selection mode**: a click released mouse capture so
the host terminal could drag-select, restored by Esc or Ctrl+C. Under
`all` its only entrance disappeared anyway, because the click went to the
guest instead. The mode was already half-dead, and the setting existed
largely to keep the other half alive.

Meanwhile every mainstream terminal has a bypass for precisely this: hold
a modifier and mouse reporting is suspended for that drag, no matter what
the running program has requested. That works without airlock's
involvement, needs no mode to enter or leave, and doesn't consume a click.
The only reason it wasn't the answer already is that nobody knows which
modifier their terminal uses — so airlock now says.

## Change

Passthrough is unconditional. Mouse events reach the sandboxed program
whenever it has mouse reporting on, and the check against what the
program asked for is retained — that is what keeps a plain shell prompt
from receiving `^[[<64;10;5M` as literal keystrokes, and keeps the
monitor's scrollback reachable there.

Selection mode is gone: `App.mouse_captured`, the two click branches that
released capture, the Esc/Ctrl+C restore path, and the `Selection mode`
footer text. Capture is now enabled once at startup and released once at
teardown.

In its place, a left click briefly shows `Hold <key> to select text` in
the same footer slot, for two seconds, refreshed by each further click.
The key comes from a small terminal-detection table: `Option` for iTerm2,
`Fn` for Terminal.app, platform-dependent for VS Code, `Shift` for
everything else. Detection is best-effort — over SSH or inside a
multiplexer `TERM_PROGRAM` may be missing or belong to another program —
so the fallback matters more than the table, and `Shift` is right almost
everywhere. The hint is recorded before the forwarding branch, so it
still appears on the common path where the click reaches the guest.

The `Mouse → sandbox` indicator went too. It existed to disclose which
mode a setting had put you in; with one behaviour there is no state to
disclose.

Two consequences worth calling out:

**Esc and Ctrl+C now always reach the guest.** Selection mode used to
swallow them, so `Ctrl+C` copied without interrupting the sandboxed
program. That interception is gone, which is the intended outcome —
Ctrl+C should interrupt — but it is a real change for anyone who had
learned the old behaviour.

**The Monitor tab's details view is affected too.** Clicking its body
also used to enter selection mode, which is how request and response
headers were copied out (`is_details_body`, now removed along with the
rect it tested against). That has nothing to do with the sandbox or with
passthrough, but the same modifier covers it and the same hint fires
there, so the flow survives in a different form.

## Config compatibility

Removing a settings key was safe to do outright: `parse_settings` carries
a comment claiming unknown fields become parse errors, but that is not
what happens — an unrecognised key is ignored. This was verified before
deleting anything rather than assumed, since every existing user's
`settings.toml` may still carry `mouse_passthrough`. A test now pins the
behaviour so a future change to strict parsing can't silently strand
those users at a startup error.
