#!/bin/bash
# Activate mise for every Bash tool command of the session: Claude Code
# sources $CLAUDE_ENV_FILE before each command.
set -euo pipefail

[[ -n "${CLAUDE_ENV_FILE:-}" ]] || exit 0
command -v mise >/dev/null || exit 0

echo 'eval "$(mise activate bash)"' >>"$CLAUDE_ENV_FILE"
