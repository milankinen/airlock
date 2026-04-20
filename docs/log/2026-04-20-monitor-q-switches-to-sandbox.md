# Monitor tab: `q` switches to Sandbox tab instead of quitting

## What

On the Monitor tab, `q` / `Q` used to signal the sandbox process to
exit (SIGHUP + SIGTERM). It now simply sets `app.active_tab =
Tab::Sandbox` — behaving as an alias for `F1`. Ctrl+D keeps the
previous exit-the-sandbox behavior.

The detail sub-tab's `q` handler got the same treatment for
consistency.

## Why

`q` = quit is a universal TUI convention, but in this app it lives
next to `F1` / `F2` tab switching, and the Monitor tab is something
users bounce in and out of while a sandbox is live. Accidentally
killing the sandbox because the muscle memory from other TUIs said
"press q to dismiss this view" was too easy and too punishing — the
previous log entry (`2026-04-18-live-policy-and-monitor-quit.md`)
called out that quitting-from-monitor was needed, but the chosen
binding turned out to be a footgun.

Re-binding `q` to "go back to the Sandbox tab" preserves the
"dismiss this view" ergonomics without risking work loss. Users who
actually want to tear down the sandbox still have Ctrl+D (or, of
course, typing `exit` in the guest shell on the Sandbox tab).

## Files touched

- `app/airlock-monitor/src/lib.rs` — both `q` branches in
  `handle_key` now set `active_tab = Tab::Sandbox`; comment above
  the main branch trimmed to describe only Ctrl+D.
- `docs/manual/src/monitor.md` — split the combined `q` / `Ctrl+D`
  row into two separate rows with their new meanings.
