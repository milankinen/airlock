# OpenAI Codex CLI: the official standalone installer, system-wide (the
# package in /usr/local/lib/codex, the command in /usr/local/bin). With
# the `acp` arg, also the ACP adapter from npm (Codex has no native ACP).
#
# Sources:
# - https://github.com/openai/codex#installing-and-running-codex-cli and
#   https://learn.chatgpt.com/docs/codex/cli (install.sh is the documented
#   first choice for macOS and Linux; npm and Homebrew are alternatives)
# - https://chatgpt.com/codex/install.sh (redirects to
#   releases.openai.com; POSIX sh; CODEX_INSTALL_DIR, CODEX_HOME,
#   CODEX_NON_INTERACTIVE; downloads the static musl package
#   codex-package-<arch>-unknown-linux-musl.tar.gz from releases.openai.com,
#   else GitHub Releases, and checks its sha256)
# - https://learn.chatgpt.com/docs/config-file/config-basic (system config
#   /etc/codex/config.toml, the lowest-precedence config file)
# - https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/features/src/lib.rs
#   (daemon_auto_start: stable, on by default)
# - https://learn.chatgpt.com/docs/sandboxing (Linux: bwrap on PATH)
# - https://github.com/openai/codex/issues/2785 (no native ACP; an
#   external adapter)
# - https://github.com/agentclientprotocol/codex-acp#installation and
#   https://github.com/agentclientprotocol/registry/blob/main/codex-acp/agent.json
#   (npm package @agentclientprotocol/codex-acp, command codex-acp; it
#   brings a compatible @openai/codex as a dependency; CODEX_PATH runs
#   another Codex binary)
# - https://github.com/agentclientprotocol/codex-acp/blob/main/src/index.ts
#   (CODEX_PATH, else the @openai/codex dependency; `codex-acp cli` runs
#   Codex with the other args)
# - The adapter is one executable from lib.sh bun_compile (Bun from
#   https://github.com/oven-sh/bun/releases, in the scratch directory
#   only). It has no Codex of its own: the pack config sets CODEX_PATH to
#   the Codex of this script.
#
# No sha pin: the installer checks the sha256 of the package it downloads;
# lib.sh bun_get checks Bun against the release's SHASUMS256.txt; Bun
# checks the registry integrity of each npm package.
# Args: acp (AIRLOCK_PACK_ARG_ACP, true or false). Without it,
# /usr/local/bin/codex-acp is a stub that exits 1 with an error.

_root=/usr/local/lib/codex
_codex=/usr/local/bin/codex
_acp=/usr/local/bin/codex-acp
_package="$_root/packages/standalone/current/codex-package.json"
# Codex writes state into CODEX_HOME at each start, even for --version,
# and CODEX_HOME must exist. The checks here use a scratch home (not
# under a temporary directory, where Codex refuses to create its helper
# links).
_check_home="$_root/check-home"
rm -rf "$_check_home"
mkdir -p "$_check_home"

# Steps: the CLI, then the ACP adapter with the `acp` arg.
if [ "${AIRLOCK_PACK_ARG_ACP:-false}" = true ]; then
    airlock_steps 2
else
    airlock_steps 1
fi

airlock_status "downloading cli"
# curl: the installer. bubblewrap: Codex runs shell commands in a bwrap
# sandbox and prefers bwrap on PATH to its bundled copy.
pkg_install ca-certificates curl bubblewrap

# The package marker: a plain codex binary (the release's single-file
# asset) is not enough, the TUI needs the complete package.
if [ -f "$_package" ] &&
    CODEX_HOME="$_check_home" "$_codex" --version </dev/null >/dev/null 2>&1 3>&-; then
    log "Codex is already installed"
else
    log "downloading the Codex installer"
    fetch https://chatgpt.com/codex/install.sh "$PACK_TMP/codex-install.sh"
    log "installing Codex"
    # CODEX_HOME is for the installer only: the package goes to a shared,
    # root-owned directory, not to ~/.codex (a host directory in airlock).
    # Codex finds its bundled rg and bwrap next to the resolved binary.
    mkdir -p "$_root"
    run_vendor "the Codex installer" env CODEX_HOME="$_root" \
        CODEX_INSTALL_DIR=/usr/local/bin CODEX_NON_INTERACTIVE=1 \
        sh "$PACK_TMP/codex-install.sh"
    # State from the installer's own `codex --version` check.
    rm -rf "$_root/tmp"
    [ -x "$_codex" ] && [ -f "$_package" ] ||
        fail 12 "the Codex installer did not create $_codex"
fi

# The Codex TUI starts a shared background server by default. That server
# copies the whole Codex package (about 384 MB) into ~/.codex, runs a
# self-updater and needs `ps -p ... -o lstart=` (procps); without it,
# `codex` exits at start. Turn it off for all users; a user's
# ~/.codex/config.toml can turn it on again.
mkdir -p /etc/codex
if [ ! -e /etc/codex/config.toml ]; then
    cat >/etc/codex/config.toml <<'TOML'
# Managed by airlock: system defaults for Codex (lowest precedence).
[features]
daemon_auto_start = false
TOML
elif ! grep -q '^[[:space:]]*daemon_auto_start' /etc/codex/config.toml; then
    log "warning: /etc/codex/config.toml exists and does not set daemon_auto_start"
fi

CODEX_HOME="$_check_home" "$_codex" --version </dev/null 3>&- ||
    fail 12 "codex --version failed"

if [ "${AIRLOCK_PACK_ARG_ACP:-false}" = true ]; then
    airlock_status "installing acp"
    bun_compile @agentclientprotocol/codex-acp codex-acp "$_acp"
    "$_acp" --version </dev/null 3>&- || fail 12 "codex-acp --version failed"
    # The Codex binary that the adapter runs: this script's binary.
    CODEX_PATH="$_codex" CODEX_HOME="$_check_home" "$_acp" cli -V </dev/null 3>&- ||
        fail 12 "codex-acp cli -V failed"
else
    acp_stub "$_acp" codex
fi
rm -rf "$_check_home"
