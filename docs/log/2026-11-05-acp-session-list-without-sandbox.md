# Empty session list for dirs without a sandbox

Editors (JetBrains AIR) list the sessions of many dirs, also of git
worktrees that the user removed. These dirs have no sandbox. The ACP
proxy sent an error for each list and logged a warning
(`agent start failed: no sandbox for ...`). The log filled with these
warnings.

## Change

* The sandbox lookup of the agent launcher returns the typed error
  `NoSandbox` when no sandbox serves the session dir. Its text is the
  same as before (`messages::cli::never_started`).
* `attach` in `acp/agent/mod.rs`: when the first request is
  `session/list` and the lookup error is `NoSandbox`, the proxy sends
  `{"sessions": []}` and ends the handle. It logs at debug level only.
* Other methods (`session/new`, `session/load`, ...) and other lookup
  errors still get the error response and the warning. The user must
  see that a new session cannot start without `airlock start`.
* `end_early` holds the shared end of a handle that did not attach.

## Why the empty list

A dir with no sandbox has no agent, thus it has no sessions. An empty
list is the true answer. It also matches the list without a session dir,
which is empty too.
