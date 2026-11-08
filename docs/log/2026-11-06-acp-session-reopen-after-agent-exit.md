# Reopen ACP sessions after the agent ended

`airlock stop` while `airlock acp` ran ended the agent of the sandbox.
The editor (JetBrains AIR) did not know it. The next prompt of the open
session then failed:

* If a session list named the session, the proxy started a new agent in
  the list dir and sent the prompt to it. The new agent did not know the
  session and replied `Resource not found`.
* Else the proxy replied that the session is not known.

A restart of `airlock acp` worked, because the editor then loads the
session before the prompt.

## Change

The router (`acp/proxy.rs`) does not change. A session handle now ends
only at `session/close`, a client load of the same session, the proxy
stop, a failed start or the end of a one-shot request. An agent exit does
not end the handle of a session with an ACP session. The handle does the
reopen (`acp/agent`):

* The session keeps the reopen data in its handle when the agent starts
  its ACP session: the session id, the host session dir and the editor
  MCP servers as the client gave them. The MCP data can have env and
  header secrets. It stays only in memory and is never logged.
* At the agent exit, the open requests get an error and the session
  shows the "agent stopped" message, as before. The handle stays.
* The next client request to the handle starts the session in the agent
  of its sandbox. This launches the agent and starts the sandbox if
  needed. The first request is a handle-made `session/resume` if the
  agent has resume, else `session/load` if it has load. The request id is
  `airlock-acp-reopen-<handle>`, in the same style as the handshake id.
* The client does not get the reopen response. For a load, it does not
  get the `session/update` messages of the session before the response,
  because the editor has the history.
* The client messages wait for the reopen response, at most 64. A client
  cancel of a waiting request goes to the agent after that request.
* If the reopen fails (no sandbox, failed start, error response), each
  waiting request gets one error
  (`messages::acp::session_reopen_failed`). The handle stays, and the
  next request tries again. An agent with no resume and no load gives
  `REOPEN_NOT_SUPPORTED`.
* If the proxy refuses its own reopen request before it goes to the
  agent (session dir check, feature check, MCP servers), the client does
  not get that error. The reopen fails with `REOPEN_REFUSED` as above.
* `session/close` of a session without an agent gets an OK response at
  once. No agent starts.
* Other client messages without an agent (notifications, responses) are
  dropped.

The proxy exit code stays the first agent exit code that is not 0. A
handle keeps the exit code of its earlier agents and gives it at its
stop.

## Why in the handle

The first version did the reopen in the router. The router had to keep
reopen data, make its own request ids and hide responses for all
sessions. The handle already has the session state and the agent link,
and the router routes the session to the same handle all the time.

## Why resume first

A resume does not send the history again. A load does, and the handle
must then drop it.
