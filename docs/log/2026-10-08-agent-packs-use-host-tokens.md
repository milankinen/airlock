# Agent packs use the API tokens of the host

## Problem

The `claude` and `codex` packs enabled only the sign-in services. A user
with `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`
on the host (or in the vault) had to add a masked `[env]` entry and an
`inject` rule by hand. The packs could not add them, because an `[env]`
entry that reads an undefined variable stops the start. Most users do
not have these tokens.

## Solution

- New `[env]` flag `optional = true`. If the template reads a variable
  that the host env and the vault do not define, the entry is left out.
  `inject` lists skip it, and a rule with no injected value is a plain
  allow rule. Template syntax errors still fail. A vault that does not
  open counts as "not defined", so an optional entry never stops a start.
- `claude` adds `CLAUDE_CODE_OAUTH_TOKEN` and `ANTHROPIC_API_KEY` as
  optional masked entries, injected on `api.anthropic.com` (rule
  `claude-tokens`). The anthropic service already accepts injected masked
  secrets on its API host.
- `codex` adds `OPENAI_API_KEY` the same way, injected by the `codex`
  rule on `api.openai.com`.
- The packs always pass the tokens that exist. A user who sets them on
  the host wants the agent to use them. Claude Code prefers them to a
  `/login` sign-in.
