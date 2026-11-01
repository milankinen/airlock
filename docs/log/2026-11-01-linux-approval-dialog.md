# Linux approval details dialog

## Goal

The macOS approval has a details window. The Linux approval had only a
short notification. A request with a long command or a rule change did not
show its full content. Linux also denied the request when the desktop had
no notification service.

## Decision

A first try called `zenity` or `kdialog`. The development host had none of
them, and a dialog program is not a safe dependency. A window of our own
needs a toolkit. GTK needs libraries on the host and a glibc build. The
CLI is a static musl binary, and the host can be NixOS, so run time
libraries are not available.

The window is a small pure Rust X11 client with software drawing:

* `x11rb` talks the X protocol directly. It needs no C library.
* `fontdue` draws one embedded monospace font (DejaVu Sans Mono) on a
  fixed grid. A fixed grid makes line breaks exact: a long command breaks
  at the column limit, so the user reads every character.
* A desktop with Wayland uses XWayland. A host with no X server gets no
  window. The request is then denied unless the notification works.

## Structure

* Crate `airlock-dialog`: protocol, text layout, drawing, user interface
  logic and the X11 window. The user interface logic does not depend on X,
  so unit tests run it without a server.
* The CLI starts the dialog as a child process of its own executable
  (`airlock __approval-dialog`). The CLI checks the argument before any
  other start step. The process reads one request on standard input and
  writes one answer on standard output. A separate process keeps the
  window loop out of the sandbox runtime. A crash of the window cannot
  stop the sandbox. The CLI kills the process when the wait ends.
* The notification gets the action "More details" when the host has
  `DISPLAY`. It opens the window. There is no "default" action for a click
  on the notification body: some servers show it as one more button.
  The notification closes before the window shows.
* If the window cannot show after "More details", the notification shows
  again with no details button, so the user can still decide.
* When the notification cannot show, the CLI opens the window directly.
* When the notification has no details button, its body does not point
  to the details.
* The child process is `/proc/self/exe`. This works also when an update
  replaced the executable file while airlock runs.
* The window shows on the monitor that has the pointer.

## Safety

* Deny is the default button. No key selects allow. Escape and a closed
  window deny.
* Clicks on buttons have no effect for 0.6 seconds after the window shows.
  A window can show under the pointer while the user clicks in another
  window. The time starts at the first draw request from the server, not
  at the map request, because a window manager can show the window late.
* Control characters in the text become `?` in the layout. The text is
  also clean before this step.

## Tests

* Unit tests: layout, scroll, drawing, clicks, keys, DPI parsing.
* End-to-end tests: `tests/test_dialog_x11.rs` starts Xvfb, runs the
  dialog, and acts as the user with the XTEST extension. The tests find
  the buttons from the pixel colors. The tests skip when the host has no
  Xvfb, except when `AIRLOCK_REQUIRE_XVFB` is set. CI sets it.

## Open points

* Native Wayland support (no XWayland).
* The window title is only in the title bar. A window manager with no
  title bar does not show it.
