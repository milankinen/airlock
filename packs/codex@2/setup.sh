# OpenAI Codex CLI from the official installer, system-wide. With the
# `acp` arg, also the codex-acp adapter (Codex has no native ACP).
# - https://github.com/openai/codex#installing-and-running-codex-cli
# - https://github.com/openai/codex/tree/main/codex-rs/hooks
#
# ~/.codex is a host mount. Thus this script writes only to /etc/codex.

_root=/usr/local/lib/codex
_codex=/usr/local/bin/codex
_acp=/usr/local/bin/codex-acp
_package="$_root/packages/standalone/current/codex-package.json"
# Codex writes state to CODEX_HOME even for --version. Codex refuses to
# make its helper links under a temporary directory, thus use this one.
_check_home="$_root/check-home"
rm -rf "$_check_home"
mkdir -p "$_check_home"

if [ "${AIRLOCK_PACK_ARG_ACP:-false}" = true ]; then airlock_steps 2; else airlock_steps 1; fi

airlock_status "installing cli"
# Codex prefers bwrap on PATH for its command sandbox.
airlock_pkg_install ca-certificates curl bubblewrap

# The TUI needs the full package, not only the binary.
if [ -f "$_package" ] &&
    CODEX_HOME="$_check_home" "$_codex" --version </dev/null >/dev/null 2>&1 3>&-; then
    airlock_log "Codex is already installed"
else
    airlock_log "installing Codex"
    airlock_fetch https://chatgpt.com/codex/install.sh "$AIRLOCK_PACK_TMPDIR/codex-install.sh"
    # CODEX_HOME puts the package in a shared root-owned directory.
    mkdir -p "$_root"
    airlock_internal__run_vendor "the Codex installer" env CODEX_HOME="$_root" \
        CODEX_INSTALL_DIR=/usr/local/bin CODEX_NON_INTERACTIVE=1 \
        sh "$AIRLOCK_PACK_TMPDIR/codex-install.sh"
    rm -rf "$_root/tmp"
    [ -x "$_codex" ] && [ -f "$_package" ] ||
        airlock_fail 12 "the Codex installer did not create $_codex"
fi

# daemon_auto_start: the server copies 384 MB into ~/.codex and needs procps.
# Codex cannot turn off the tool timeout, thus one day for host approvals.
# "approve" skips the Codex prompt, because airlock asks the user itself.
cat >"$AIRLOCK_PACK_TMPDIR/config.toml" <<'TOML'
# Managed by airlock (codex pack). Put your settings in ~/.codex/config.toml.
[features]
daemon_auto_start = false

[mcp_servers.airlock]
url = "http://admin.airlock/mcp"
startup_timeout_sec = 30
tool_timeout_sec = 86400
default_tools_approval_mode = "approve"
omit_tools_from = ["deferred"]
TOML

# A hook failure must not break Codex. Codex accepts only JSON or empty
# output, thus curl -f (no body on HTTP error) and `|| true`.
_c="curl -fsS --noproxy '*' -H 'Content-Type: application/json' -X POST --data-binary @- --max-time"
_u=http://admin.airlock/codex/hooks
cat >"$AIRLOCK_PACK_TMPDIR/hooks.json" <<JSON
{"hooks": {
  "SessionStart": [{"hooks": [{"type": "command", "command": "$_c 10 $_u/session-start || true", "timeout": 15, "additionalContextLimit": 0}]}],
  "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "$_c 5 $_u/pre-tool-use || true", "timeout": 10}]}],
  "PostToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "$_c 5 $_u/post-tool-use || true", "timeout": 10}]}]
}}
JSON

# Args: new file, target file, airlock mark in the target.
# A target with no mark comes from the user or another pack. Keep it,
# because this script cannot merge it.
_install() {
    if [ -e "$2" ] && ! grep -qF "$3" "$2" 3>&-; then
        airlock_log "warning: $2 is not from airlock. It stays, without the airlock settings"
        return 0
    fi
    # Rename, so Codex never reads a partial file.
    { cp "$1" "$2.new" && chmod 644 "$2.new" && mv -f "$2.new" "$2"; } ||
        airlock_fail 12 "could not write $2"
}
mkdir -p /etc/codex
_install "$AIRLOCK_PACK_TMPDIR/config.toml" /etc/codex/config.toml "Managed by airlock"
_install "$AIRLOCK_PACK_TMPDIR/hooks.json" /etc/codex/hooks.json "$_u/"

CODEX_HOME="$_check_home" "$_codex" --version </dev/null 3>&- ||
    airlock_fail 12 "codex --version failed"

if [ "${AIRLOCK_PACK_ARG_ACP:-false}" = true ]; then
    airlock_status "installing acp"
    airlock_internal__bun_compile @agentclientprotocol/codex-acp codex-acp "$_acp"
    "$_acp" --version </dev/null 3>&- || airlock_fail 12 "codex-acp --version failed"
    CODEX_PATH="$_codex" CODEX_HOME="$_check_home" "$_acp" cli -V </dev/null 3>&- ||
        airlock_fail 12 "codex-acp cli -V failed"
else
    airlock_internal__acp_stub "$_acp" codex
fi
rm -rf "$_check_home"
