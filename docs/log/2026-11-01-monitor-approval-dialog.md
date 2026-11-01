# Approval dialog in the monitor TUI

Temporary network access and rule changes need the user's approval. The
host asks with a desktop notification and an X11 window (Linux), or with
the notifier app and a system dialog (macOS). When the host could not
show anything, airlock denied the request at once. Now the monitor TUI
shows the request when the host cannot, if `airlock start --monitor`
runs. Order: system UI, then the monitor dialog, then deny.

## Shown or unavailable

The platform asks now return either a shown result (a decision) or an
unavailable result (nothing showed). Only unavailable goes to the monitor.
A timeout or a dismiss on the desktop is a decision, so the monitor is
not asked after it.

* Linux: unavailable only when the notification cannot show and then
  the X11 details window cannot open. The notification cannot show on a
  capabilities error or timeout, with no `actions` capability, or on a
  show error before it displays. The window cannot open with no X
  display, or when its process fails. A show timeout uses the full time,
  so it stays a timeout decision. When the window fails after "More
  details", the notification shows again, as before.
* macOS: unavailable only when the notification fails and then the
  `osascript` dialog does not start or exits with an error. The dialog
  has no cancel button, so an error exit means that no dialog showed.

All paths use one deadline. The monitor gets only the time that is left.
With no time left, no dialog shows.

## Connection to the monitor

The TUI runs on its own thread. The CLI side runs on a single-thread
runtime with a LocalSet. A shared slot (`MonitorSlot`) holds the dialog
sender while the TUI runs. The monitor runtime makes the slot, fills it
at launch and clears it at exit and on drop. `airlock start` makes the
approver right after it selects the runtime and gives it the slot. The
grant context gets only the approver. The raw terminal runtime gives no
slot, so the approver falls through to deny. A small trait in
airlock-cli wraps the sender, so the tests can use a fake dialog.

The request goes to the TUI with a oneshot reply channel. `true` allows,
`false` denies. On timeout the CLI drops the receiver. The TUI checks the
channel before each draw and removes a closed request, so a late answer
has no effect. A send error or a closed channel before an answer denies.

The monitor path has no running notice with an abort button. The command
still ends at the end of `network.grants.max_duration`.

## Dialog

The dialog draws last, on top of any tab, and takes all input: keys,
pastes and mouse events. Requests wait in a FIFO queue and show one at a
time.

* Short form: the same title and body as the desktop notification, and
  the buttons `accept`, `deny` and `show more`. The first letter of each
  label is its shortcut, with a color. A left click on the title or the
  body shows the details, as a click on the notification opens the
  details window. Thus the "Click for more details" body is true in the
  TUI too. The click is a toggle, not an answer.
* Padding: 1 empty row above the content, 2 columns at the sides, 2
  rows between the content and the buttons, 1 row below the buttons.
  On a small screen the padding goes away first.
* Long form: the title and the same detail items as the macOS window,
  with no body. Diff lines are green or red. The content scrolls in both
  directions (arrows, `hjkl`, page keys, Home/End, wheel, Shift+wheel).
  The buttons stay visible.
* Enter does nothing. Esc only hides the details.

Protections against an answer that the user did not mean:

* Accept and deny wait 300 ms after a request shows, and 1 s when the
  request shows right after an answer. Without the kitty keyboard
  protocol, a held key sends repeats as presses. The first repeat comes
  250-660 ms after the press, so 300 ms alone does not stop it.
* `a` and `d` count only after 150 ms with no other key. Key repeat
  events and keys with modifiers other than Shift do nothing.
* Accept and show more need at least one drawn content row for the
  current request. A terminal that is too small shows a note and only
  the deny button.
* A mouse press that started in the guest sends its release to the
  guest also when a dialog opened. A press on the dialog does not send
  its release to the guest after the dialog closes.
