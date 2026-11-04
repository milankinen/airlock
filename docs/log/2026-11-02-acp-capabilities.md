# ACP capabilities

Since `airlock acp`, IDEA shows less than with a direct agent. It lost
sub-agents, session history and other details.

## Cause

- Restrict mode dropped all client capabilities except
  `elicitation.form`, `session` and `auth`. It dropped `_meta` and
  `subagents`, so the agents thought the editor cannot show sub-agents.
- The proxy answered `initialize` with a fixed, minimal result (session
  load, image, embedded context, steering). IDEA enables its UI from the
  capabilities of the agent.

## Change

- Client capability allow-list in restrict mode. It keeps
  `elicitation.form`, `session`, `subagents`,
  `plan`, `_meta.terminal_output`, `_meta.terminal_output_delta` and
  `_meta["subagent-transcript"]`. Of `_meta.jetbrains.air` it keeps the
  integer `version` and the safe `capabilities` names. It drops `fs`,
  `terminal`, `auth`, `elicitation.url`, `terminal-auth`, `planFile` and
  `agentFileChangeReport`, because the editor or the agent could act on
  host paths or log in on the host. `--pass-client-capabilities` is
  unchanged.
- The agent message filter also forwards `_auth/status_update` and
  `_session/list/changes`. Both carry display data.
- Capability presets for `claude-agent-acp`, `codex-acp` and
  `copilot --acp`. The proxy matches the base name of the command before
  the login shell wrap. Other commands get the old reply.
- `--set-agent-capabilities=<list>` replaces the preset. Names are ACP
  field paths, plus `steering`, `air:<name>` and `air:goal`. An unknown
  name gives exit code 2. AIR parts go into the reply only when the
  client declared AIR. `air:sessionIndex` adds its extras only when the
  client declared `sessionIndex`.
- The proxy checks client requests against the `initialize` result of the
  real agent with a presence test. It refuses `session/list`, `resume`,
  `close`, `delete`, `fork`, `_session/goal`,
  `_session/async_task/stop` and the session index methods with
  `METHOD_NOT_FOUND` when the agent lacks the feature. The proxy logs the
  features that it offered and the agent lacks. The proxy always refuses
  `authenticate`, `logout` and `providers/*` with `METHOD_NOT_FOUND`.
- `session/list` without `cwd` gets an empty list.
- `session/list` rows carry the guest `cwd`. The proxy maps it back to
  the host directory for each request, in both modes. Paths with `..`
  and other paths stay as they are.

## Known gaps

- The proxy does not map `additionalDirectories`, `_session/batch_get`
  rows, `_session/list/changes` and `session_info_update` paths.
- The tool call `terminalId` comes from the agent.
- The client `auth` feature is fully dropped. Its terminal login runs on
  the host, and its gateway login gives gateway credentials to the
  sandbox. No feature set has `auth.logout` or `providers`, because the
  proxy refuses `logout` and `providers/*`. A logout would sign out all
  sandboxes of the agent pack.
- Launchers such as `npx` get the default preset. Users must set
  `--set-agent-capabilities`.
- Presets can become old when the agents change.
