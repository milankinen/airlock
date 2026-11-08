# ACP session routing review fixes

Six review findings in the `airlock acp` proxy, after the change to one
agent per session (`2026-11-07-acp-agent-per-session.md`). The agent is
not trusted.

## Fixes

* Load and resume claim only the session id that the client asked for.
  Before, the agent could answer a load with the live session id of
  another project and take it over (`replace` claim). A different
  `result.sessionId` is now ignored with a debug log. New and fork still
  claim the agent id, with the conflict check.
* Agent request ids carry the agent run. The session task gives each
  agent request the id `{run}-{id}` before the proxy adds its handle
  prefix, and maps a client answer back only if the request is open in
  the current run. When an agent ends, the task sends the editor a
  `$/cancel_request` for each open request and starts a new run. Before,
  a new agent restarted its ids under the same handle id, so an answer in
  an old dialog (for example "Allow") reached a new request. The task
  also adds the run to the request id of an agent cancel.
* An agent `$/cancel_request` goes to the editor only for an open request
  of the same handle, with the id that the editor knows. The proxy finds
  it in its agent request map and removes it.
* The warn log of a failed agent start hides the MCP `env` and `headers`
  values before it cuts the params.
* The hidden reopen request has a time limit (`handshake_timeout`, no
  better config exists). A timeout uses the reopen failure path: the
  waiting requests get an error and the handle stays for a retry.
* Only an OK close of the claimed session ends the session. The session
  task knows its claimed session, so it ends itself (normal stop path:
  agent stop, MCP release, `Ended`) at the OK response, or at once when
  the agent is gone. A close of another session of the handle (for
  example a sub-agent session) or a failed close changes nothing. The
  proxy no longer looks at close responses.

## Tests

One end-to-end test per fix. Each test fails without its fix. The log
test uses a new thread-local log capture helper in `test_cfg/acp.rs`.
The old test that expected a failed close to stop the handle now uses an
OK close.

## Follow-up: cancel at handle stop

A handle stop (close of the main session, load of an open session, proxy
shutdown) dropped the open agent requests without a word. The editor kept
their dialogs open. The session task now cancels them at each agent end,
also the last one. The proxy keeps its agent request map entries until
the handle ends, so these last cancels still pass its check.

## Follow-up: less code

The first version put the runs, the cancels and the close handling in
the proxy (an extra handle message, per-handle run and claim maps, an
async handle stop). The session task owns its one agent and knows its
runs and its claimed session, so these parts moved there. The proxy
changes are now only the agent cancel check and the later removal of the
agent request entries.

The log capture helper keeps a second tracing dispatcher alive. With one
live dispatcher, tracing treats it as the global one. Another test thread
then caches "never" for a log site that it hits first, and the capturing
thread loses events (the log test failed about one run in four).
