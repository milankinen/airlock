# ACP multi-sandbox

An editor reuses one `airlock acp` process for many projects. The proxy
must serve sessions in many sandboxes through one client connection.

## Problem

- ACP allows many sessions in one agent process. Each session has its
  own absolute `cwd`.
- The `cwd` comes only in the session requests. The client waits for the
  `initialize` reply before it sends them.
- An agent runs in one VM. One agent cannot serve two sandboxes.
- Sessions of different projects must not see each other.

## Design

One agent per sandbox in the router was tried and replaced by session
handles.

- Handshake: the proxy answers `initialize` itself, with the features of
  the agent preset. It keeps the first client params for all agents.
- Router (`proxy.rs`): reads the client, writes the client, and routes
  by session id. It knows only session handles. It keeps the handle of
  each session id.
- Session handle: one for each session request. It has its own identity.
  The agent factory creates it. A handle ends alone. The end of a
  handle ends only its sessions.
- Shared process (`agent/shared.rs`): one agent process for each
  sandbox. The first session in a sandbox launches it. It does one
  agent handshake. Then it gives the session requests of all its
  handles to the agent, with guest paths. When the last handle detaches,
  the agent input closes and the agent stops. A process that does not
  exit in the grace time is killed.
- Sandbox start: the launch finds the sandbox of the session dir and
  gets its sandbox process (`Sandbox::run` with `Run::Acp`). It starts
  a stopped sandbox with the config of the last `airlock start`. The
  agent process holds one connection to the sandbox until it exits.
  Thus the sandbox runs while the sandbox has an agent, and stops when
  the agent and all other connections end. A sandbox that never
  started gives the "never started" error. A sandbox process that stops
  in its init gives the session request its last error line, and its
  exit code and output in the error `data`. The guest paths come after
  the connect, because a start at the same time can change them. A
  lookup of a session dir (`locate`) never starts the sandbox.
- A `session/list` with a `cwd` also starts a stopped sandbox, and the
  sandbox stops after the list if nothing else uses it. This is
  intended: an editor that lists the sessions of many projects starts
  their sandboxes, and the starts run at the same time.
- Agent messages: a response goes to the handle of its request. A
  message with a session id goes to the handle that owns the id. A new
  id, for example of a sub-agent, goes to the handle with the last open
  prompt, else to the oldest handle. A message without a session id
  goes to the oldest handle.
- One-shot handles: a request that is not a session request runs in a
  one-shot handle. It owns no ACP session and gets no agent message
  except the response. It ends after the response. A one-shot handle
  can be the last handle, then the agent stops.
  - `session/list` with a `cwd` runs in a one-shot handle in the sandbox
    of that `cwd`. The rows get host paths. The router keeps the session
    ids of the rows with the `cwd` of the list, at most 4096.
  - A request for a listed session that has no handle, for example
    `session/delete`, runs in a one-shot handle in the `cwd` of the
    list. A completed delete forgets the session.
  - `session/list` without `cwd` gets an empty list.
- Not supported: `authenticate`, `logout` and `providers/*` get
  `METHOD_NOT_FOUND`. A request without a session id gets the same
  error.

### Isolation rules

- A handle gets only the client messages of its own sessions.
- The router renames the id of each agent request to `<handle>-<id>`.
  It maps the client answer back to the original id and handle.
- The router drops an agent request, cancel or notification for a
  session of another handle.
- A session id belongs to one handle. A claim of an id that another
  handle owns gets an error, unless the client asked for it with load or
  resume.
- A `session/close` response ends the handle. A load or resume of an
  open session stops the old handle, then starts a new one.
- A stopping handle keeps the sessions that it had at its stop. It
  writes only in them, and gets no new session. Its last output still
  reaches the client.
- A session `cwd` outside the sandbox of the handle gets an error.

### Exit and signals

- An agent exit ends all sessions of that agent. Open requests get an
  error. A session that started gets a notice, or an agent message if
  the client has no `session.notices`. The start of a session again
  launches a new agent.
- The proxy runs until the client closes.
- A signal after a handle got its session stops all handles and waits
  for them, so that the agents can save their sessions. A second signal
  kills the agents. A signal before that kills the agents and gives
  exit code 128 + signal number.
- The exit code is the first exit code of an agent that is not 0.
- Limits: a too long agent output line, or an agent that does not read
  its input, stops the agent. The agent stderr is dropped when the host
  stderr is slow.

### Logs

- `info`: the client features as sent and after the filter, the session
  starts, the agent start and exit.
- `debug`: the method of each dropped or refused agent message, and
  refused client requests.
- `trace`: each line between airlock and an agent as `<from>: <line>`
  with the sandbox. `from` is `agent` or `airlock`. MCP `env` and header
  values are hidden.

## Known gaps

- The router guesses the handle of a new session id and of a message
  without a session id. In a sandbox with many sessions, a wrong guess
  can show a sub-agent in the wrong session.
- The router does not map `additionalDirectories` in requests.
- List rows with a dir outside the project dir keep their path.
- A sandbox start for a session or a list needs the vault. With a
  locked vault and no `AIRLOCK_VAULT_PASSPHRASE`, the request gets an
  error.
- One MCP registration stalls the client reading for at most
  `mcp_register_timeout`.
- A client batch is dropped. ACP has no batches.
- In pass mode, the proxy does not check session ids outside
  `params.sessionId`.
