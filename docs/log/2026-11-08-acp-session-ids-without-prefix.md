# ACP session ids without the sandbox prefix

`airlock acp` no longer adds the `<sandbox id>:` prefix to the ACP
session ids. The editor sees the ids of the agent as they are. This
reverts the prefix part of `2026-11-08-acp-sandbox-session-ids.md`.

## Why

The sandbox id is the registry id of the sandbox. A new sandbox for the
same project gets a new id. After that, all session ids that the editor
kept have the old prefix, and the editor cannot load or resume them. The
agent still has the sessions, for example in `~/.claude`.

## Design

* Agent messages go to the client with their session ids as they are.
  An agent message whose `params.sessionId` or `result.sessionId` is not
  a string is still dropped.
* Client messages go to the agent with their session ids as they are.
  The "not a session of this project" error is gone.
* The other session checks stay. The router gives a new session id to
  the first handle that uses it (`owns()`, `claim()`). A load or a resume
  opens only the session that the client asked for. Thus an agent cannot
  take a session that is open in another handle.
* A client load or resume of an open session moves the session to the
  sandbox of the load dir and stops the old handle, also when that dir
  is in another project. Before, the prefix check refused such a load.
  The editor is trusted, so this is accepted.

## Known limit

All sandboxes of one `airlock acp` process share one session id space.
An agent can use a session id that is not open, also an id of another
project, for example from a shared agent config. Normal agents make
unique ids and do not do this. The manual tells users to run a different
`airlock acp` process for each sandbox when the agent is not trusted.
