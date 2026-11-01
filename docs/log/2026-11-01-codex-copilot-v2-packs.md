# codex@2 and copilot@2 packs

`claude@2` gives Claude Code the airlock MCP server, the tool hooks and
the session instructions. `codex@2` and `copilot@2` give the same to
Codex and the Copilot CLI. New projects get version 2. Version 1 stays
available. Both agents mount their home dir (`~/.codex`, `~/.copilot`)
from the host, thus the packs write only system-level files or flags.

## Codex (checked against rust-v0.166.0)

* `/etc/codex/config.toml` is the system config layer (lowest
  precedence, deep merge with `~/.codex/config.toml`). The pack owns the
  file and writes `[mcp_servers.airlock]` with
  `default_tools_approval_mode = "approve"`, `omit_tools_from =
  ["deferred"]` (no tool search before the first use) and
  `tool_timeout_sec = 86400`. Codex cannot turn off the tool time limit
  (unset = 300 s), and `execute` waits for the user.
* `/etc/codex/hooks.json`: system hooks are managed, thus trusted with no
  prompt. Only command hooks exist, so curl sends stdin to airlockd.
  `additionalContextLimit = 0` on SessionStart, else Codex puts text over
  2500 tokens into a file.
* A file without the airlock mark stays, with a warning. The script
  cannot merge foreign TOML or JSON.
* Codex has no failure event, and the shell result has no exit code.
  `/codex/hooks/post-tool-use` thus ends the tool window and reports the
  blocked hosts after each call, with `hookEventName = "PostToolUse"`.
  Codex refuses output with an other event name or unknown fields, so
  the Claude failure route cannot be used. Pre-tool-use and
  session-start use the Claude handlers, because the formats are the
  same.
* Shell commands in the Codex sandbox (workspace-write) have no network.
  airlock sees no denies from them. `codex --yolo` turns that sandbox
  off.

## Copilot (checked against 1.0.105)

* No system config can add MCP servers. `/usr/local/bin/copilot` is now a
  wrapper that adds `--additional-mcp-config=@<root-owned file>` and
  `--allow-tool=airlock`. The real executable is in
  `/usr/local/libexec/copilot` (or the node package on musl aarch64).
  Checks with the release: subcommands, `--help`, `--version` and
  `--acp` accept the flags. The `=` form keeps `--allow-tool` from
  taking the next args. An upgrade from copilot@1 moves the executable
  and does not download it again.
* MCP `timeout` is in ms and must be positive, thus 24 h.
* Hooks: `/etc/github-copilot/policy.d/50-airlock.json` (root-owned, not
  group or world writable, load with no folder trust). http hooks must
  use https, thus command hooks with curl. Each command ends with
  `|| true`, because a failed preToolUse command denies the tool.
* The hook payloads have no tool call id (github/copilot-cli#5261).
  airlockd keys a call by a hash of session id, tool name and the
  arguments with sorted object keys. A string argument value is parsed
  as JSON first. Two same calls at the same time share one window.
* Copilot reads a flat `{"additionalContext": ...}`. Its MCP tool names
  are `airlock-execute` and so on, thus the instructions use the
  `airlock-` prefix.

## Not verified

`tests/vm/packs.bats` now installs the v2 agent packs and checks their
files. It did not run yet: the build host has no KVM. Open points:
Copilot plain-http MCP to `admin.airlock`, policy hooks in a real
session, `hookSpecificOutput`-free output on `postToolUseFailure`.
