# Claude Code: the official native installer, with the binary moved to
# /usr/local/bin for every user. With the `acp` arg, also the official
# ACP adapter from npm.
#
# Sources:
# - https://code.claude.com/docs/en/setup (native install with
#   https://claude.ai/install.sh; launcher ~/.local/bin/claude, a symlink
#   into ~/.local/share/claude/versions/; Alpine/musl: bash, curl, libgcc,
#   libstdc++, ripgrep and USE_BUILTIN_RIPGREP=0; DISABLE_UPDATES also
#   blocks `claude install`)
# - https://claude.ai/install.sh (bash; picks the -musl build on musl,
#   checks the binary against the release manifest's sha256, then runs
#   `claude install`, which writes only under $HOME)
# - https://code.claude.com/docs/en/troubleshoot-install (install paths;
#   the installer scans the whole disk when it starts in /)
# - https://code.claude.com/docs/en/env-vars (DISABLE_AUTOUPDATER,
#   DISABLE_UPDATES, DISABLE_INSTALLATION_CHECKS, USE_BUILTIN_RIPGREP)
# - https://www.npmjs.com/package/@agentclientprotocol/claude-agent-acp
#   (command claude-agent-acp; it was @zed-industries/claude-code-acp
#   before)
# - https://github.com/agentclientprotocol/registry/blob/main/claude-acp/agent.json
# - https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts
#   (claudeCliPath: CLAUDE_CODE_EXECUTABLE, else the Claude Code binary
#   from an optional dependency of @anthropic-ai/claude-agent-sdk; `--cli`
#   runs that binary with the other args)
# - The adapter is one executable from lib.sh bun_compile (Bun from
#   https://github.com/oven-sh/bun/releases, in the scratch directory
#   only). It has no Claude Code of its own: the pack config sets
#   CLAUDE_CODE_EXECUTABLE to the binary of this script.
#
# No sha pin: the installer checks the sha256 of the binary it downloads;
# lib.sh bun_get checks Bun against the release's SHASUMS256.txt; Bun
# checks the registry integrity of each npm package.
# Args: acp (AIRLOCK_PACK_ARG_ACP, true or false). Without it,
# /usr/local/bin/claude-agent-acp is a stub that exits 1 with an error.
# The pack config sets USE_BUILTIN_RIPGREP=0 (the ripgrep from this
# script), DISABLE_AUTOUPDATER=1 and DISABLE_INSTALLATION_CHECKS=1 (no
# warnings about the moved binary). It mounts ~/.claude from the host,
# not ~/.claude.json: this script creates that file.

_claude=/usr/local/bin/claude
_acp=/usr/local/bin/claude-agent-acp

# Steps: the CLI, then the ACP adapter with the `acp` arg.
if [ "${AIRLOCK_PACK_ARG_ACP:-false}" = true ]; then
    airlock_steps 2
else
    airlock_steps 1
fi

airlock_status "downloading cli"
# bash and curl: the installer. libgcc and libstdc++: the musl build.
# ripgrep: the search tool of Claude Code.
case "$DISTRO" in
    alpine) pkg_install bash curl ca-certificates libgcc libstdc++ ripgrep ;;
    debian) pkg_install bash curl ca-certificates ripgrep ;;
esac

if "$_claude" --version </dev/null >/dev/null 2>&1 3>&-; then
    log "Claude Code is already installed"
else
    log "downloading the Claude Code installer"
    fetch https://claude.ai/install.sh "$PACK_TMP/claude-install.sh"
    log "installing Claude Code"
    # A scratch HOME: the installer writes only there. Not in /: the
    # installer then scans the whole disk. DISABLE_UPDATES (config env)
    # turns `claude install` into a no-op that exits 0. The config's
    # DISABLE_INSTALLATION_CHECKS is for the installed binary only.
    mkdir -p "$PACK_TMP/home"
    (
        unset XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME CLAUDE_CONFIG_DIR \
            DISABLE_UPDATES DISABLE_INSTALLATION_CHECKS SUDO_USER
        HOME=$PACK_TMP/home
        export HOME
        cd "$HOME"
        exec bash "$PACK_TMP/claude-install.sh"
    ) </dev/null 3>&- || fail 12 "the Claude Code installer failed"
    _binary=$(readlink -f "$PACK_TMP/home/.local/bin/claude" 3>&-) || _binary=
    [ -f "$_binary" ] ||
        fail 12 "the Claude Code installer did not create ~/.local/bin/claude"
    chmod 755 "$_binary"
    mkdir -p /usr/local/bin
    mv -f "$_binary" "$_claude"
fi

"$_claude" --version </dev/null 3>&- || fail 12 "claude --version failed"

# ~/.claude.json is not a host mount: each sandbox has its own. A new one
# skips the onboarding; an existing one stays.
if [ ! -e "$HOME/.claude.json" ]; then
    log "creating $HOME/.claude.json"
    printf '%s' '{"hasCompletedOnboarding":true}' >"$HOME/.claude.json" ||
        fail 12 "could not create $HOME/.claude.json"
fi

if [ "${AIRLOCK_PACK_ARG_ACP:-false}" = true ]; then
    airlock_status "installing acp"
    bun_compile @agentclientprotocol/claude-agent-acp claude-agent-acp "$_acp"
    "$_acp" --version </dev/null 3>&- || fail 12 "claude-agent-acp --version failed"
    # The Claude Code binary that the adapter runs: this script's binary.
    CLAUDE_CODE_EXECUTABLE="$_claude" "$_acp" --cli --version </dev/null 3>&- ||
        fail 12 "claude-agent-acp --cli --version failed"
else
    acp_stub "$_acp" claude
fi
