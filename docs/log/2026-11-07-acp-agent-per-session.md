# One ACP agent process per session

`airlock acp` now runs one agent process for each session handle, also
for the one-shot handles of `session/list` and of a listed-session
delete. This reverses the shared agent of each sandbox from
`2026-11-04-acp-multi-sandbox.md`.

## Why

* The shared agent (`acp/agent/shared.rs`, ~1100 lines) needed routing
  tables (owners, prompt order, members), a link protocol between the
  handles and the agent task, and many state enums (handle state,
  reopen slot, join, late, drain, dropped). The reopen after an agent
  exit (`2026-11-06-acp-session-reopen-after-agent-exit.md`), for the
  `airlock stop` of a sandbox with open sessions, made these states hard
  to follow and to fix.
* The router had to guess the session of an agent message with a new
  session id, for example of a sub-agent, and of a message without a
  session id. A wrong guess showed the message in the wrong editor
  session. This closes the "guesses the session" gap of the 2026-11-04
  entry: an agent now has one session, and the proxy `owns()` check
  still gives sub-agent session ids to that handle.

## Design

* `acp/agent/session.rs`: one async task per handle owns all state as
  local variables. State changes are control flow, not enums. The task
  loops over agents: launch, handshake, relay, and at an agent exit
  wait for the next client request, then launch again and reopen.
* `Handle { tx, task }`: `send` puts the message into an unbounded
  inbox. `shutdown` drops `tx`. The task reads the inbox to its end, so
  the stop is in-band: it comes after the earlier messages, in every
  state, also during the launch or an MCP registration. A panicked task
  gives exit code 1.
* `acp/agent/process.rs` holds one running agent (input, kill, limits,
  handshake check, reopen method). `acp/agent/output.rs` gives the agent
  output as a stream of lines, too-long items and the exit.
* The reopen now lives in the session task. After the handshake of the
  new agent the task sends the hidden resume or load request first. The
  client messages wait for its response, at most 64. A failed reopen
  gives the waiting requests an error and the next request tries again.
* The handshake time limit starts before the launch, so it includes the
  sandbox start, as before.
* The editor MCP servers of the session are released when the agent is
  lost, and again at the session end.
* The logs of the session task and its notice task are in a
  `session{handle=N}` span. The MCP release and the launcher relay tasks
  log without it. The sandbox name no longer identifies an agent.

## Exit code

Each handle gives the first exit code that is not 0 of its agents. The
proxy keeps the first such code in the order the handles end. Codes
count only after the agent handshake. The stop of a one-shot agent after
its response does not count. A one-shot agent that exits by itself
before its response counts.

## Behavior changes

* All requests that wait at a failed start get the start error, not
  only the first request.
* Requests that wait when the proxy stops the session during the
  launch get "the agent of this session stopped".
* A failed `session/load` or `session/resume` no longer claims the
  session (bug found in review of the first version).
* `SESSION_DETACHED` is gone. Open requests at a stop get "the agent
  in sandbox 'x' exited (code N)".

## Trade-offs

* N sessions = N agent processes, handshakes and sandbox connections.
  A second session in a sandbox waits for its own agent start. Each
  session list starts a short agent.
* Agents that keep sessions only in memory fail fork, load and delete
  in a new process. Claude and Codex store sessions on disk.
* Load of an open session: the proxy stops the old handle and starts
  the new one at once. The new agent can load while the old agent
  still saves at the end of its input. Claude saves each message at
  once, so this is accepted.
* Agents can write their config files in parallel.

## Tests

`test_shared_agent.rs` is deleted. `test_session_agents.rs` replaces
`test_multi_sandbox.rs`. New tests: list beside an open session, one-shot
list exit code 0, refused load claims no session, reopen not supported,
reopen queue limit, fork with its own handshake.
