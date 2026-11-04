# Approval dialogs

Some requests from the sandbox need your approval on the host:

- a [temporary network grant](./networking.md#temporary-network-grants)
  for one command, with a list of hosts or an on-demand rule
- an add or a change of a network rule

Airlock shows each request to you and waits for Allow or Deny. The agent
in the sandbox writes the text of the request. Thus airlock does not
trust this text. Before the text shows, airlock removes terminal escape
sequences, control characters and invisible characters, and cuts long
values.

## Order of the dialogs

By default, airlock asks in this order:

1. A desktop notification with the buttons Allow, Deny and More details.
   If the notification cannot show, a window asks on Linux, and a system
   dialog asks on macOS.
2. A dialog in the [monitor TUI](../usage/monitor.md). Airlock uses it
   only if the host cannot show the request and a client of
   `airlock start --monitor` is attached. The monitor dialog gets only
   the rest of the time limit.
3. If nothing can show the request, airlock denies it.

On Linux, the host cannot show the request when the notification cannot
show and the window cannot open. On macOS, the host cannot show the
request when the notification and the system dialog both fail.

More details opens a window that shows the whole request:

- the sandbox
- the full command
- the working directory
- the extra environment
- the hosts or the rule
- the time limit

For a rule change, the window shows the lines that change in color.

Airlock shows one request at a time. Other requests wait in a queue.

## The `approvals.dialogs` setting

The `approvals.dialogs` key in your
[user settings](../configuration.md#user-settings)
(`~/.airlock/settings.toml`) sets where airlock asks:

```toml
[approvals]
dialogs = "system"
```

- `"system"` (default): the order above.
- `"monitor"`: only the monitor TUI. Airlock does not show a desktop
  notification, the Linux window or a system dialog. If the monitor TUI
  is not attached, airlock denies the request.
- `"none"`: no dialogs. Airlock denies all requests.

## macOS

Notification Center accepts notifications only from an app bundle. Thus
a small notifier app (`airlock-macos-notifier`) shows the notification.
The airlock binary contains the app. On first use, airlock installs the
app to `support/` in the [data directory](./project-layout.md#data-directory).
Airlock installs the app again when the app version changes. Airlock
unpacks the new app to a temporary directory and then moves it into
place, so that no process sees a partial app. A lock lets only one
airlock process install at a time. Airlock registers the app with the
system, so that the notification shows the name and the icon of the app.

Airlock starts the app as a child process. Airlock sends the request
with a time limit on the standard input. The app sends one answer on the
standard output. The time limit of the app ends 2 seconds before the
limit of airlock. In this time, the app removes the notification and
sends the timeout answer. When airlock closes the input, the app removes
the notification and stops.

The notification permission:

- Before you decide on the permission, macOS asks for it only when the
  app starts through the system app launcher (`open`). Then the app tells
  airlock about this, and airlock starts the app again through the
  launcher.
- If you set the alert style of the app to none, the notification does
  not show. Then the app does not show it and airlock uses the system
  dialog.
- If the alert style is banner, the notification goes away after a short
  time. Airlock writes a hint to the log: set the notifications of
  Airlock to "Alerts" in System Settings > Notifications > Airlock.

A click on the notification has the same effect as More details. The
details window shows the details as plain text, with the buttons Allow
and Deny. The Return key selects Deny. A close of the window is a deny.
The window closes when the time limit ends.

When the notification cannot show, airlock shows a system dialog through
`osascript`. Airlock gives the request text to the dialog script as
arguments. Thus the text cannot change the script. Deny is the default
button, so the Return key denies. The dialog closes when the time limit
ends.

## Linux

Airlock sends the notification through the notification service of the
desktop (the D-Bus notification protocol). The service must support
buttons. If the service gives no answer in 5 seconds, airlock uses the
next dialog. The notification has critical urgency and stays until you
answer. Airlock removes the notification after the answer or the
timeout.

The details window comes from `airlock-dialog`. It is an X11 window and
it has no C library dependency. Thus the same build runs on all Linux
hosts. The window needs an X server. A desktop with Wayland needs
XWayland. The window runs in a child process of airlock, so that a
failure of the window cannot stop airlock. Airlock sends the request on
the standard input and reads the answer on the standard output.

- With no X server (`DISPLAY` is not set), the notification has no More
  details button.
- If the window cannot open after you select More details, the
  notification shows again without the button.
- If the notification cannot show, for example when the desktop has no
  notification service, the window asks you.

In the window:

- The Deny button is the default. No key selects Allow.
- Escape and the close button of the window deny the request.
- The window stays above other windows and shows a countdown.
- The mouse wheel, the arrow keys, Page Up, Page Down, Home and End
  scroll long text.

## Monitor TUI

The monitor dialog shows on top of the current tab. It has the same text
as the notification, and the buttons **accept**, **deny** and **show
more**. Click the text, or press `s`, to see all details. The dialog
takes all input: no key, paste or mouse event goes to the sandbox or to
the tabs.

| Key                  | Action                                |
|----------------------|---------------------------------------|
| `a`                  | Accept the request                    |
| `d`                  | Deny the request                      |
| `s`                  | Show or hide all details              |
| `Esc`                | Hide the details. It does not answer. |
| Arrow keys, `hjkl`   | Scroll                                |
| `PgUp` / `PgDn`      | Scroll one page                       |
| `Home` / `End`       | Go to the top left, or to the end     |

If the terminal is too small to show the request, the dialog shows a
note and only the deny button.

## Security measures

- Default deny: no answer in time, a closed notification, a closed
  window, an error, or no way to ask all mean deny.
- Time limit: `network.grants.approval_timeout` sets the time to answer.
  The default is 120 seconds. All dialogs together get this time.
- Late answers: when the time limit ends, airlock stops the wait and
  closes the dialog. A late answer has no effect.
- Deny cooldown: after you deny a request, airlock denies new requests
  for 30 seconds with no dialog. A timeout or a closed notification does
  not start the cooldown.
- Queue limit: at most 4 requests wait behind the open request. Airlock
  denies more requests.
- Misclick protection: a key or a click that you aim at a different
  window must not allow a request.
  - The Linux window ignores clicks for 0.6 seconds after it shows.
  - The monitor dialog ignores answers for 0.3 seconds after it shows,
    and for one second after you answer the previous request.
  - The monitor dialog ignores `a` and `d` when they come less than
    0.15 seconds after the previous key, for example when you hold a
    key.
  - `Enter` does nothing in the monitor dialog. No key selects Allow in
    the Linux window. Return selects Deny on macOS.

## Running notice

After you allow a command, the host shows a notice with an Abort button
while the command runs. Abort ends the grant and closes its connections.
The notice goes away when the command ends. On macOS, only a direct
start of the notifier app shows the notice. A start through the app
launcher has no notice. The monitor TUI does not show the notice.
