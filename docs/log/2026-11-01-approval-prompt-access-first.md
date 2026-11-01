# Show the requested access before the command in approval prompts

## Problem

A security review found a spoof risk in the approval prompt. The agent in
the sandbox writes the command text. The details showed the command first,
then the hosts or the rule, then the duration. Cleaning kept new lines in
the command, up to 4000 characters. Lines of blank-looking characters, for
example U+2800 (braille blank) and U+3164 (Hangul filler), stayed, because
they are not whitespace and not format characters.

An agent could thus write a fake `Hosts:` block in the command, then about
2000 blank-looking lines. The real "Hosts: all hosts" section was then far
below. The macOS osascript fallback dialog shows the details as plain text,
with no host summary, so this was the most direct case. The X11 dialog was
also open to it, with only the label color as a cue.

## Fix

All front-ends (macOS notifier details, osascript dialog, Linux X11 dialog,
monitor TUI) show the details in the order that the prompt text gives. So
the fix is in one place:

* Order: sandbox, hosts or rule, duration, then command, working directory
  and extra environment. The agent text now comes after the access that
  the host enforces.
* The command shows at most 20 lines, then a "# and N more lines" line.
  The character limit stays.
* The cleaning removes U+115F, U+1160, U+17B4, U+17B5, U+2800, U+3164 and
  U+FFA0. Lines of only such characters are empty and go away.

## Notes

* Keep agent-written text after the enforced access in the details. Do not
  add a front-end that sorts the details in its own order.
