# approvals.dialogs user setting

The user setting `[approvals] dialogs` selects where approval requests
show:

* `system` (default): the desktop first, then the monitor dialog, then
  deny.
* `monitor`: no desktop notification and no host window or dialog. The
  monitor dialog shows if the monitor runs. Otherwise the request is
  denied.
* `none`: deny all requests. No dialog shows.

The setting is personal, not part of the project config, because it
depends on the user's desktop and work habits, not on the project. It
uses the same enum setting pattern as the other settings, so an unknown
value stops the settings load. `airlock start` reads the value one time,
right after it selects the terminal runtime, and makes the approver for
it. The grant context gets the approver, not the setting. `none` gives
an approver that always denies. `monitor` gives an approver with no
platform ask, or one that always denies with the raw terminal. Thus
`monitor` and `none` never start the notifier app, the Linux window or
the dialog script. The deny reasons name the key and its value, so the
user can find the cause in the log.
