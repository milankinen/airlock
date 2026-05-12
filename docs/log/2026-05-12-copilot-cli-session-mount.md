# Fix Copilot CLI preset mount configuration

The `copilot-cli` preset had an incorrect and unnecessary mount. It mounted
`~/.airlock/copilot` → `~/.config/gh` — the GitHub CLI config directory —
even though Copilot CLI doesn't use `~/.config/gh` and doesn't require `gh`
to be installed. The actual persistent state Copilot CLI writes lives in
`~/.copilot`.

## What changed

Replaced the incorrect `~/.config/gh` mount with the correct one:
`~/.airlock/copilot-cli` → `~/.copilot` (shadow copy, `missing = "create-dir"`).

This enables Copilot CLI session state to persist across sandboxes:
- `config.json` — User settings and preferences
- `session.db` — Session state, login data, and interaction history
- `logs/` and `analytics/` — Activity and telemetry tracking

The `~/.config/gh` mount was removed entirely. A dedicated `github-cli` preset
can be added in the future for users who want to run `gh` inside the sandbox.

The middleware `target` was narrowed from `*.githubcopilot.com` to the
explicit API hosts (`api.githubcopilot.com`, `api.business.githubcopilot.com`)
so the token is not sent to telemetry endpoints. The network allow list
still uses `*.githubcopilot.com` — telemetry traffic flows but without
the user's credential.

## Documentation

Rewrote the "Providing the GitHub token" section:
- Named link to PAT creation with the correct permission label
  ("Account > Copilot Requests")
- Clarified the placeholder/injection model: the sandbox only ever sees a
  placeholder value in `COPILOT_GITHUB_TOKEN`; airlock swaps in the real
  credential at the host boundary. The previous text was contradictory —
  it said Copilot "picks up the token" and also that "the real token never
  enters the sandbox".
- Added a reference link to the official Copilot CLI authentication docs.
