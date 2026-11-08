# ACP proxy: simplifications and agent limits

A second full review of the `airlock acp` code, after
`2026-11-07-acp-review-fixes.md`. Four simplifications first, then two
bug fixes. The agent is not trusted.

## Simplifications

* One id rewrite for agent requests. The session task gives each agent
  request the id `<hid>.<run>.<n>` and keeps the original id until the
  answer. The router finds the handle of an editor answer from the
  number before the first `.`, and keeps no agent request map. Before,
  the session and the router both renamed the id, and the editor saw ids
  such as `0-"0-\"ok1\""`. The session checks agent cancels against its
  own open requests.
* The hidden reopen request is not in the open client requests. Its
  response is found first. The error paths need no reopen filter. The
  router drops an error for the reopen id because no client request has
  that id.
* No session dir lookup in other sandboxes. The router starts a new
  handle for each request with a session dir (new, fork, load, resume,
  list with a dir). Thus only the first request and the reopen of a
  session have a `cwd`, and it is the session dir. The launcher `locate`
  and the "other sandbox" check could not run. A different `cwd` now
  gets an error.
* One MCP server set per session. The registry had a map of sessions to
  keys, key reuse for loads and a pending close. A session now has its
  own agent, so it has at most one set: `Option<ServerSet>` and the id of
  the request that made it. The set ends at an error response to that
  request, and with the agent or the session. The close of the claimed
  session ends the session, so it needs no own release.

## Fixes

* Memory limits for agent ids. A handle gets at most 64 ACP sessions
  (`owns()`), and a new session id has at most 256 bytes. A session keeps
  at most 64 open agent requests, with ids of at most 256 bytes. A
  refused request gets an error response, so the agent does not wait. An
  agent request in a session of another agent also gets an error now,
  not a silent drop. The router sends the error through the handle, and
  the session maps the id back as for an editor answer.
* A session request that starts no session stops its agent. Before, the
  agent and its sandbox kept running after:
  * an error response or a response without a session id: the session
    task ends the agent at that response
  * a refusal of the first request by the proxy, for all session kinds
    (before: only one-shot)
  * a session id conflict in the router: the router stops the handle
    after it sends the error

## Tests

Each fix has an end-to-end test that fails without it:
`test_agent_limits.rs`, `test_session_routing.rs`
(`agent_request_for_other_session_gets_error`) and
`test_failed_session_request.rs`. The tests of the agent request ids use
real fake-agent processes now, because the session task does the rename.

## Second review

### Fixes

* Two loads or resumes of one session before the first answer: neither
  stopped the other at its start, because the session had no owner yet.
  Both claimed it with `replace`, and the first agent ran until the
  proxy ended. Now a `replace` claim stops the live handle that had the
  session.
* The Claude and Codex presets no longer offer `air:sessionIndex`. Its
  requests have no session id, so the router cannot send them to the
  agent of one session.
* Listed sessions: the queue of listed ids made repeated row ids cost
  O(n²) on a list line of up to 16 MB, and row ids had no length limit.
  Now a `quick_cache::unsync::Cache` with `MAX_LISTED` entries keeps
  them. A list gives at most `MAX_LISTED` rows, and ids over
  `MAX_ID_BYTES` are skipped. The map is global for the proxy and the
  last list wins: a session in two lists keeps the dir of the later
  list (known limit).
* Agent text in log messages (reopen error detail, a refused session id,
  trace lines) is written in debug form, so control characters are
  escaped. Fields were escaped already by the log format.
* Agent lines that are not one JSON object (for example a batch) are
  dropped in all modes. Before, pass mode sent them to the editor without
  checks. The router also drops a handle message that is not JSON-RPC.

### Simplifications

* No `former` map: the sessions of a stopping handle stay mapped to it
  until it ends. A live handle can take such a session, for example
  after a load. `on_handle_end` removes the entries that still point to
  the handle.
* One `Config` for the router and the agents (no `ProxyConfig`,
  `AgentConfig`, `split()`). The CLI `Options` stay, because they also
  hold the sandbox launcher settings.
* One-step factory: the router keeps the handle settings
  (`Option<Rc<SessionContext>>`) from the client handshake and gives
  them to `SessionFactory::create`. No `SessionFactoryInit` and
  `FactoryState`.
* Agent kill: a one-shot channel instead of a control enum, an unbounded
  channel and a relay task loop.
* Session state: `reopening: bool`, and one `First` enum for the kind of
  the first request (new, load or resume, one-shot with its method).
* Small: no dead `$/cancel_request` entry in the agent notification list
  (cancels have their own kind), no duplicate Codex notification list,
  `ServerSet::default()`, no stderr drop counter (drops are logged at
  debug level), a fixed module doc wrap.

### Known limits

* Agent notifications without a session id reach the editor from any
  agent.
* The proxy shutdown can hang if the editor stops reading its output: the
  last messages wait for the output queue.
* An agent exit that a reopen recovered from still makes the proxy exit
  code not 0.
* Session requests can offer `additionalDirectories`, but the sandbox
  does not mount them.

## Third review

### Fixes

* Session ids of other projects: see
  `2026-11-08-acp-sandbox-session-ids.md`. The editor sees
  `<sandbox>:<id>`, a load or resume gives the session to its new handle
  at once, and the session of a stopping handle is not free.
* A load or resume starts its new handle before it stops the old one. A
  refused request (bad dir, shutdown) keeps the old session.
* While a session opens again, every `session/update` with a session id
  is hidden, also of sub-agent sessions.
* After a failed reopen, the agent is muted before its stop. A late OK
  answer to the reopen and other late output do not reach the editor.
  No test: the late answer needs a timing that the fake agent cannot
  make sure of.
* A reopen whose sandbox does not start keeps the error data (exit code,
  output) in the error to the waiting requests.
* Editor answers and cancels of agent requests go to the agent at once.
  They do not wait for the reopen or an MCP registration. The router
  routes an editor cancel of an agent request by the handle in its id.
* The AIR session index is gone fully: client names, reply extras,
  request rules and the `_session/list/changes` notification.
* In the default mode, the session, sub-agent and plan client features
  keep only their known parts (`session.configOptions`,
  `session.notices`).
* The router counts the live handles that completed a handshake. A
  signal after the end of all of them stops the proxy at once with
  128 + the signal number.

### Simplifications

* Agent request ids are `<hid>.<n>`. The counter of the session is over
  all its agents, so no run number is needed.
* No `handshake` copy in the router: a second `initialize` gets the
  `offered` result of the handle settings.
* One `keep_names` helper for the safe client `_meta` names.
* No replace path in the MCP registry: a session has one registration
  per agent.
* One router check for agent messages in sessions of other handles.
