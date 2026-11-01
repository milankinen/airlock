# Claude Code: the official native installer, with the binary in
# ~/.local/share/claude/versions. A wrapper at ~/.local/bin/claude and
# /usr/local/bin/claude runs the newest version there. With the `acp`
# arg, also the official ACP adapter from npm.
#
# Sources:
# - https://code.claude.com/docs/en/setup (native install with
#   https://claude.ai/install.sh, launcher ~/.local/bin/claude, a symlink
#   into ~/.local/share/claude/versions/. Alpine/musl: bash, curl, libgcc,
#   libstdc++, ripgrep and USE_BUILTIN_RIPGREP=0. DISABLE_UPDATES also
#   blocks `claude install`.)
# - https://code.claude.com/docs/en/getting-started/installation#auto-updates
#   (updates write ~/.local/share/claude/versions/<version>. Since
#   v2.1.207, they do not replace a custom ~/.local/bin/claude.)
# - https://claude.ai/install.sh (bash. It picks the -musl build on musl,
#   checks the binary against the sha256 of the release manifest, then
#   runs `claude install`, which writes only under $HOME.)
# - https://code.claude.com/docs/en/troubleshoot-install (install paths.
#   The installer scans the whole disk when it starts in /.)
# - https://code.claude.com/docs/en/env-vars (DISABLE_AUTOUPDATER,
#   DISABLE_UPDATES, DISABLE_INSTALLATION_CHECKS, USE_BUILTIN_RIPGREP)
# - https://www.npmjs.com/package/@agentclientprotocol/claude-agent-acp
#   (command claude-agent-acp, before that @zed-industries/claude-code-acp)
# - https://github.com/agentclientprotocol/registry/blob/main/claude-acp/agent.json
# - https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts
#   (claudeCliPath: CLAUDE_CODE_EXECUTABLE, else the Claude Code binary
#   from an optional dependency of @anthropic-ai/claude-agent-sdk. `--cli`
#   runs that binary with the other args.)
# - The adapter is one executable from lib.sh
#   airlock_internal__bun_compile (Bun from
#   https://github.com/oven-sh/bun/releases, in the scratch directory
#   only). It has no Claude Code of its own. The pack config sets
#   CLAUDE_CODE_EXECUTABLE to the wrapper of this script.
#
# No sha pin: the installer checks the sha256 of the binary it downloads.
# lib.sh airlock_internal__bun_get checks Bun against the SHASUMS256.txt of
# the release. Bun checks the registry integrity of each npm package.
# Args: acp (AIRLOCK_PACK_ARG_ACP, true or false). Without it,
# /usr/local/bin/claude-agent-acp is a stub that exits 1 with an error.
# The pack config sets USE_BUILTIN_RIPGREP=0 (the ripgrep from this
# script), DISABLE_AUTOUPDATER=1 and DISABLE_INSTALLATION_CHECKS=1 (no
# warnings about the wrapper). It mounts ~/.claude from the host, not
# ~/.claude.json. This script creates that file.
#
# Differences to claude@1: the airlock MCP server in ~/.claude.json, and
# the airlock hooks and MCP permissions in a managed settings drop-in.
# Sources:
# - https://code.claude.com/docs/en/managed-settings (Linux: /etc/claude-code/
#   managed-settings.json and managed-settings.d/*.json, merged in
#   alphabetical order. Lists combine.)
# - https://code.claude.com/docs/en/hooks (http hooks. Hooks of all sources
#   run. Only allowedHttpHookUrls, if set, limits the URLs. SessionStart
#   runs only command hooks. Their JSON stdout with
#   hookSpecificOutput.additionalContext goes into the context.)
# - https://code.claude.com/docs/en/mcp (user scope is in ~/.claude.json.
#   http entry: {"type":"http","url":...})
# - https://code.claude.com/docs/en/permissions (rule mcp__<server>__<tool>)

_claude=/usr/local/bin/claude
_acp=/usr/local/bin/claude-agent-acp
_versions=$HOME/.local/share/claude/versions

# Steps: the CLI, then the ACP adapter with the `acp` arg.
if [ "${AIRLOCK_PACK_ARG_ACP:-false}" = true ]; then
    airlock_steps 2
else
    airlock_steps 1
fi

airlock_status "installing cli"
# bash and curl: the installer. libgcc and libstdc++: the musl build.
# ripgrep: the search tool of Claude Code.
case "$DISTRO" in
    alpine) airlock_pkg_install bash curl ca-certificates libgcc libstdc++ ripgrep ;;
    debian) airlock_pkg_install bash curl ca-certificates ripgrep ;;
esac
# jq: the settings edits below.
airlock_pkg_install jq

# The wrapper at ~/.local/bin/claude exists only after a complete install.
if "$HOME/.local/bin/claude" --version </dev/null >/dev/null 2>&1 3>&-; then
    airlock_log "Claude Code is already installed"
else
    airlock_log "downloading the Claude Code installer"
    airlock_fetch https://claude.ai/install.sh "$AIRLOCK_PACK_TMPDIR/claude-install.sh"
    airlock_log "installing Claude Code"
    # A scratch HOME, because the installer writes only there. Not in /,
    # because there the installer scans the whole disk. A DISABLE_UPDATES
    # from the config env makes `claude install` do nothing and exit 0.
    # The DISABLE_INSTALLATION_CHECKS of the config is only for the
    # installed binary. Thus unset both.
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
    # Keep only the binary. The wrapper below replaces the launcher.
    chmod 755 "$_binary"
    mkdir -p "$_versions"
    mv -f "$_binary" "$_versions/${_binary##*/}"
fi

# The wrapper. Claude Code updates add versions to $_versions and keep
# this wrapper. It does not run `claude` from PATH, thus it cannot run
# itself. Claude Code copies with wl-copy only when WAYLAND_DISPLAY is
# set. The wrapper sets it only when airlock grants clipboard copy (the
# copy FIFO exists). Else Claude Code uses its other copy methods.
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

# ~/.claude.json is not a host mount. Each sandbox has its own. A new
# file skips the onboarding. An existing file stays.
if [ ! -e "$HOME/.claude.json" ]; then
    airlock_log "creating $HOME/.claude.json"
    printf '%s' '{"hasCompletedOnboarding":true}' >"$HOME/.claude.json" ||
        airlock_fail 12 "could not create $HOME/.claude.json"
fi

# The airlock MCP server (tools execute,
# show_rules and upsert_rule). `claude mcp add --scope
# user` writes only this entry to ~/.claude.json. This script writes the
# same entry with jq. It does not start Claude Code, which can ask for
# onboarding or sign-in. The write goes into the existing file (`cat >`),
# so the owner and mode stay. A broken file stays as it is.
_mcp='{"type":"http","url":"http://admin.airlock/mcp"}'
jq --argjson s "$_mcp" '.mcpServers.airlock = $s' "$HOME/.claude.json" \
    >"$AIRLOCK_PACK_TMPDIR/claude.json" 3>&- ||
    airlock_fail 12 "could not update $HOME/.claude.json"
cat "$AIRLOCK_PACK_TMPDIR/claude.json" >"$HOME/.claude.json" ||
    airlock_fail 12 "could not write $HOME/.claude.json"

# Managed settings: a drop-in file of this pack. It is the only file that
# this script owns, so a new run replaces it. managed-settings.json and
# the files of other tools stay as they are. Claude Code merges hooks and
# permission lists of all sources. The hooks send tool events to the
# airlock admin server. The SessionStart hook gets the instructions for the
# airlock tools. The text shows only the features that the host enabled.
# Claude Code skips http hooks for SessionStart, so curl sends the request.
# The permissions let the agent call the MCP tools without a Claude Code
# prompt, because airlock asks the user itself.
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

# Older versions of this pack wrote the tool instructions to the managed
# memory file /etc/claude-code/CLAUDE.md, between two marker lines. The
# SessionStart hook replaces them. Remove that block and keep all other
# content. An unclosed begin marker removes the rest of the file.
_memory=/etc/claude-code/CLAUDE.md
if [ -f "$_memory" ] && grep -qx '<!-- airlock:begin -->' "$_memory" 3>&-; then
    awk '
      $0 == "<!-- airlock:begin -->" { skip = 1; next }
      skip { if ($0 == "<!-- airlock:end -->") skip = 0; next }
      { print }' "$_memory" >"$AIRLOCK_PACK_TMPDIR/memory-new" 3>&- ||
        airlock_fail 12 "could not build $_memory"
    cat "$AIRLOCK_PACK_TMPDIR/memory-new" >"$_memory" ||
        airlock_fail 12 "could not write $_memory"
fi

if [ "${AIRLOCK_PACK_ARG_ACP:-false}" = true ]; then
    airlock_status "installing acp"
    airlock_internal__bun_compile @agentclientprotocol/claude-agent-acp claude-agent-acp "$_acp"
    "$_acp" --version </dev/null 3>&- || airlock_fail 12 "claude-agent-acp --version failed"
    # The adapter runs the Claude Code wrapper of this script.
    CLAUDE_CODE_EXECUTABLE="$_claude" "$_acp" --cli --version </dev/null 3>&- ||
        airlock_fail 12 "claude-agent-acp --cli --version failed"
else
    airlock_internal__acp_stub "$_acp" claude
fi
