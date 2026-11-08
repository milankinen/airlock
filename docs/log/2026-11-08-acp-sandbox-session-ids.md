# ACP session ids with a sandbox prefix

`airlock acp` now shows the editor each ACP session id as
`<sandbox id>:<agent session id>`. The sandbox id is the registry id of
the sandbox, which is stable for a project.

## Why

The agents of all projects run as the same user and often share their
config, for example `~/.claude`. An agent can thus read the session ids
of another project. Before, the router gave each new session id from an
agent to the handle of that agent (`owns()`), and a load or a resume in
one project could name a session of another project. An agent could
thus take the session of another project: get its prompts and send
messages in it.

## Design

* The session task adds `<sandbox>:` to every session id that goes to
  the editor: each string value in `params` or `result`, at any depth,
  whose key is `sessionId` or ends in `SessionId` (for example
  `parentSessionId` and the rows of a session list). It does this at
  once after it reads an agent line, so all later checks (claim, owns,
  list records, notices) use the editor form. Values of other types stay
  as they are: tool input and output can have such keys with any value.
  Only an agent message whose `params.sessionId` or `result.sessionId`
  is not a string is dropped.
* It removes the prefix from `params` of each client request and
  notification just before it goes to the agent, also for the hidden
  reopen request. `params.sessionId` must have the prefix of the sandbox
  of the agent: else a request gets an `INVALID_PARAMS` error and a
  notification is dropped. This fails closed. Nested ids lose the prefix
  only if they have the prefix of this sandbox, else they stay as they
  are. Client answers to agent requests stay as they are.
* An agent can only make ids with its own prefix. Thus `owns()` needs no
  extra check: the agents of one sandbox can share ids (one project), the
  agents of two sandboxes cannot.
* A load or a resume gives the session to its new handle when the new
  session task accepted the load: after the prefix and the other checks
  of the first request, before the request goes to the agent
  (`HandleMessage::Opening`). The router then stops the old handle. The
  last output of the old handle can no longer take the session in the
  gap before the new claim. A load that the new handle refuses (for
  example the id of another project) keeps the old handle and its
  session. A session of a stopping handle is not free for other
  handles. `Claim` lost its `replace` flag: a claim only fails when
  another handle has the id.
* An idle session (its agent ended, no new agent yet) tells the router
  with `HandleMessage::Idle`. A signal then has no agent to let save,
  and stops the proxy at once with 128 + the signal number.

## Effect on the editor

The session ids that the editor sees change format. Sessions that the
editor kept from before this change (for example in its session history)
have ids without a prefix. A load or resume of such an id now gets the
error "not a session of this project". A new session list from the agent
gives the ids in the new format, and the editor can load them from
there.

## Known limit

The router stops the old handle when the new handle accepts the load,
before the new handle registers the editor MCP servers. If that
registration fails, the load gets an error, and the old session is gone
too. The editor can load the session again.
