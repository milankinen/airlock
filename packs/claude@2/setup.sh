# Claude Code from the official native installer, a wrapper that runs the
# newest installed version, the airlock MCP server and hooks. With the
# `acp` arg, also the ACP adapter.
#
# Sources:
# - https://code.claude.com/docs/en/setup
# - https://code.claude.com/docs/en/env-vars
# - https://code.claude.com/docs/en/managed-settings
# - https://code.claude.com/docs/en/hooks
# - https://github.com/agentclientprotocol/claude-agent-acp
#
# No sha pin: the installer checks the sha256 of the CLI. Bun checks the
# npm integrity of the adapter.

_claude=/usr/local/bin/claude
_acp=/usr/local/bin/claude-agent-acp
_versions=$HOME/.local/share/claude/versions

if [ "${AIRLOCK_PACK_ARG_ACP:-false}" = true ]; then
    airlock_steps 2
else
    airlock_steps 1
fi

airlock_status "installing cli"
# libgcc and libstdc++: the musl build.
case "$DISTRO" in
    alpine) airlock_pkg_install bash curl ca-certificates libgcc libstdc++ ripgrep ;;
    debian) airlock_pkg_install bash curl ca-certificates ripgrep ;;
esac
airlock_pkg_install jq

if "$HOME/.local/bin/claude" --version </dev/null >/dev/null 2>&1 3>&-; then
    airlock_log "Claude Code is already installed"
else
    airlock_log "downloading the Claude Code installer"
    airlock_fetch https://claude.ai/install.sh "$AIRLOCK_PACK_TMPDIR/claude-install.sh"
    airlock_log "installing Claude Code"
    # A scratch HOME: the installer scans the whole disk when it starts in
    # /. DISABLE_UPDATES makes `claude install` do nothing.
    mkdir -p "$AIRLOCK_PACK_TMPDIR/home"
    (
        unset XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME CLAUDE_CONFIG_DIR \
            DISABLE_UPDATES DISABLE_INSTALLATION_CHECKS SUDO_USER
        HOME=$AIRLOCK_PACK_TMPDIR/home
        export HOME
        cd "$HOME"
        exec bash "$AIRLOCK_PACK_TMPDIR/claude-install.sh"
    ) </dev/null 3>&- || airlock_fail 12 "the Claude Code installer failed"
    _binary=$(readlink -f "$AIRLOCK_PACK_TMPDIR/home/.local/bin/claude" 3>&-) || _binary=
    [ -f "$_binary" ] ||
        airlock_fail 12 "the Claude Code installer did not create ~/.local/bin/claude"
    chmod 755 "$_binary"
    mkdir -p "$_versions"
    mv -f "$_binary" "$_versions/${_binary##*/}"
fi

# Claude Code updates add versions and keep the wrapper. WAYLAND_DISPLAY
# only when airlock grants clipboard copy.
cat >"$AIRLOCK_PACK_TMPDIR/claude" <<'WRAPPER'
#!/bin/sh
# Managed by airlock (claude pack): runs the newest Claude Code.
dir=$HOME/.local/share/claude/versions
v=$(ls "$dir" 2>/dev/null | grep -x '[0-9][0-9.]*' | sort -V | tail -n 1)
[ -n "$v" ] || { echo "airlock: no Claude Code in $dir" >&2; exit 127; }
if [ -p /run/airlock/clipboard.copy ]; then
    export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-airlock-0}"
else
    unset WAYLAND_DISPLAY
fi
exec "$dir/$v" "$@"
WRAPPER
for _f in "$HOME/.local/bin/claude" "$_claude"; do
    mkdir -p "${_f%/*}"
    cp "$AIRLOCK_PACK_TMPDIR/claude" "$_f.new"
    chmod 755 "$_f.new"
    mv -f "$_f.new" "$_f"
done

"$_claude" --version </dev/null 3>&- || airlock_fail 12 "claude --version failed"

# A new file skips the onboarding.
if [ ! -e "$HOME/.claude.json" ]; then
    airlock_log "creating $HOME/.claude.json"
    printf '%s' '{"hasCompletedOnboarding":true}' >"$HOME/.claude.json" ||
        airlock_fail 12 "could not create $HOME/.claude.json"
fi

# Same entry as `claude mcp add --scope user`, without a Claude Code start.
# `cat >` keeps the owner and mode of the file.
_mcp='{"type":"http","url":"http://admin.airlock/mcp"}'
jq --argjson s "$_mcp" '.mcpServers.airlock = $s' "$HOME/.claude.json" \
    >"$AIRLOCK_PACK_TMPDIR/claude.json" 3>&- ||
    airlock_fail 12 "could not update $HOME/.claude.json"
cat "$AIRLOCK_PACK_TMPDIR/claude.json" >"$HOME/.claude.json" ||
    airlock_fail 12 "could not write $HOME/.claude.json"

# Managed settings drop-in. Claude Code merges hooks and permissions of all
# sources. SessionStart runs only command hooks, thus curl. The MCP tools
# need no Claude Code prompt, because airlock asks the user.
mkdir -p /etc/claude-code/managed-settings.d
jq -n '
  def hook(u): [{matcher: "*", hooks: [{type: "http", url: u}]}];
  {
    hooks: {
      SessionStart: [{hooks: [{
        type: "command",
        command: "curl -fsS --noproxy \"*\" --max-time 10 -X POST http://admin.airlock/claude/hooks/session-start"
      }]}],
      PreToolUse: hook("http://admin.airlock/claude/hooks/pre-tool-use"),
      PostToolUse: hook("http://admin.airlock/claude/hooks/post-tool-use"),
      PostToolUseFailure:
        hook("http://admin.airlock/claude/hooks/post-tool-use-failure")
    },
    permissions: {
      allow: [
        "mcp__airlock__execute",
        "mcp__airlock__show_rules",
        "mcp__airlock__upsert_rule",
        "mcp__airlock__list_manual_entries",
        "mcp__airlock__get_manual_entry"
      ]
    }
  }' >"$AIRLOCK_PACK_TMPDIR/airlock-managed.json" 3>&- ||
    airlock_fail 12 "could not build the managed settings"
cat "$AIRLOCK_PACK_TMPDIR/airlock-managed.json" \
    >/etc/claude-code/managed-settings.d/50-airlock.json ||
    airlock_fail 12 "could not write the managed settings"

if [ "${AIRLOCK_PACK_ARG_ACP:-false}" = true ]; then
    airlock_status "installing acp"
    airlock_internal__bun_compile @agentclientprotocol/claude-agent-acp claude-agent-acp "$_acp"
    "$_acp" --version </dev/null 3>&- || airlock_fail 12 "claude-agent-acp --version failed"
    CLAUDE_CODE_EXECUTABLE="$_claude" "$_acp" --cli --version </dev/null 3>&- ||
        airlock_fail 12 "claude-agent-acp --cli --version failed"
else
    airlock_internal__acp_stub "$_acp" claude
fi
