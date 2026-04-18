# Monitor tab bar: status line + theme-friendly colors

Two adjustments to the bottom tab bar and the Monitor tab's widgets,
rolled together because they share a goal — make the TUI readable on
both dark and light terminal themes, and surface sandbox state at a
glance without having to switch to F2.

## Status line

The bottom tab bar now carries right-aligned indicators on the same
row as `F1 Sandbox` / `F2 Monitor`:

```
 F1 Sandbox   F2 Monitor                  CPU 12% │ Memory 2.3 GiB / 8.0 GiB │ Network 42 1
```

Values come directly from the already-running `pollStats` pipeline
and the network counters the Monitor tab already tracks — no new
state. The Monitor tab's `(count)` badge is gone: the status line
carries the same information and then some, and the title text stays
still regardless of traffic volume.

Allowed/denied counts are shown as request counts only
(`request_allowed` / `request_denied`), not the combined total that
the old badge used. The combined figure mixed TCP connects with HTTP
requests — which double-counts, since every HTTP request sits inside
a TCP connection — and made the bar busier than it needed to be.

## Color refresh

Several widgets used `Color::White` or `Color::Gray` as the "primary
text" color. On light themes that renders as literal white-on-white
or near-invisible gray. Replaced with `Span::raw` (= default fg) for
any spans that should read as the user's normal text color:

- Request `Endpoint` column and Connection `Target` column.
- Requests / Connections / Details sub-tab labels (both active and
  inactive states; active keeps Bold + Underlined as the active-tab
  tell).
- Policy dropdown rows, and the dropdown itself no longer forces a
  `bg(Color::Black)` — it inherits the terminal's default bg.
- Request / Connection details pane values (Received, Target, Path,
  header values, Connected, Disconnected, and the "Headers"
  heading).

Status colors (green / red), the `Method` accelerator (cyan), and
low-emphasis labels (`DarkGray`) are unchanged — those are semantic
and readable on either theme.

F1/F2 hotkey tint moved from `Color::Yellow` to `Color::Cyan` to
match the `R` / `C` / `p` accelerator letters already used inside
the network panel, so the vocabulary of "this letter is a shortcut"
stays uniform across the TUI.

## Status-line label color

The `CPU` / `Memory` / `Network` labels in the status line use
`Color::Gray` (matching the unselected `Sandbox` / `Monitor` tab
labels); the values use `Color::DarkGray`. This inverts the usual
"dim label, bright value" pattern — the intent is for the labels to
be the scan landmarks while the values stay understated, since the
bar is peripheral information, not the focus of the screen.
