# GitHub Copilot CLI from the GitHub releases, for all users. The wrapper
# /usr/local/bin/copilot adds the airlock MCP server.
# - https://github.com/github/copilot-cli/releases
# - https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference
# - https://docs.github.com/en/copilot/reference/hooks-reference
#
# ~/.copilot is a host mount. Thus the MCP config goes in wrapper flags
# and the hooks go in a policy file in /etc.

_bin=/usr/local/bin/copilot
_real=/usr/local/libexec/copilot
_lib=/usr/local/lib/copilot
_mcp=/usr/local/share/airlock/copilot-mcp.json
_policy=/etc/github-copilot/policy.d/50-airlock.json
_tmp=$AIRLOCK_PACK_TMPDIR

case "$ARCH" in
    x86_64) _arch=x64 ;;
    aarch64) _arch=arm64 ;;
    *) airlock_fail 10 "unsupported architecture: $ARCH" ;;
esac
_plat=linux
[ "$LIBC" = musl ] && _plat=linuxmusl
# The musl arm64 executable cannot load its native addons. Only
# --version works. Thus run the npm package of the release on node.
_node=
[ "$_plat-$_arch" = linuxmusl-arm64 ] && _node=1
_exec=$_real
[ -n "$_node" ] && _exec="/usr/bin/node $_lib/index.js"

# <mode> <file> <text>: write a root-owned file. The rename prevents
# partial reads.
_put() {
    { mkdir -p "${2%/*}" && printf '%s\n' "$3" >"$2.new" &&
        chmod "$1" "$2.new" && mv -f "$2.new" "$2"; } ||
        airlock_fail 12 "could not write $2"
}

# Run the wrapper with all state in the scratch directory. --help loads
# the native addon, --version does not. Neither reads the MCP config.
_check() {
    HOME="$_tmp/home" XDG_CACHE_HOME="$_tmp/cache" COPILOT_AUTO_UPDATE=false \
        "$_bin" --help </dev/null >/dev/null 2>&1 3>&-
}

airlock_steps 1
airlock_status "installing cli"
# bash: the Copilot shell tool and the hooks. curl: the hooks.
case "$DISTRO" in
    alpine) airlock_pkg_install bash ca-certificates curl ${_node:+nodejs} ;;
    debian) airlock_pkg_install bash ca-certificates curl libstdc++6 libgcc-s1 ;;
esac

# copilot@1 put the executable at $_bin. Keep it, if it is not a wrapper.
if [ -z "$_node" ] && [ ! -e "$_real" ] && [ -f "$_bin" ] &&
    [ "$(head -c 2 "$_bin")" != "#!" ]; then
    airlock_log "moving the Copilot CLI of copilot@1 to $_real"
    mkdir -p "${_real%/*}"
    mv -f "$_bin" "$_real"
fi

# The execute tool waits for the user on the host, thus 24 h. The timeout
# is in ms, and Copilot refuses 0.
_put 644 "$_mcp" '{"mcpServers":{"airlock":{"type":"http","url":"http://admin.airlock/mcp","tools":["*"],"timeout":86400000}}}'

# Policy hooks load with no folder trust. http hooks must use https, thus
# curl. Each command exits 0, because a failed preToolUse denies the tool.
_hook() {
    printf '[{"type":"command","bash":"curl -fsS --noproxy \\"*\\" --max-time %s -H \\"Content-Type: application/json\\" --data-binary @- http://admin.airlock/copilot/hooks/%s || true","timeoutSec":%s}]' \
        "$2" "$1" "$(($2 + 5))"
}
mkdir -p "${_policy%/*}"
chmod 755 /etc/github-copilot "${_policy%/*}"
_put 644 "$_policy" "{\"version\":1,\"hooks\":{
\"sessionStart\":$(_hook session-start 10),
\"preToolUse\":$(_hook pre-tool-use 5),
\"postToolUse\":$(_hook post-tool-use 5),
\"postToolUseFailure\":$(_hook post-tool-use-failure 5)}}"

# The flags go before the user args, thus subcommands also work. With
# `=`, --allow-tool does not take the next arg. airlock asks the user
# itself, thus Copilot does not ask before the airlock tools.
_put 755 "$_bin" "#!/bin/sh
# Managed by airlock (copilot pack).
exec $_exec --additional-mcp-config=@$_mcp --allow-tool=airlock \"\$@\""

if _check; then
    airlock_log "Copilot CLI is already installed"
else
    airlock_internal__github_latest_tag github/copilot-cli
    case "$TAG" in
        v[0-9]*) ;;
        *) airlock_fail 12 "unexpected latest Copilot CLI release: $TAG" ;;
    esac
    if [ -z "$_node" ]; then
        _asset=copilot-$_plat-$_arch.tar.gz
    else
        _asset=github-copilot-${TAG#v}-$_plat-$_arch.tgz
    fi
    airlock_log "downloading Copilot CLI $TAG"
    _url=https://github.com/github/copilot-cli/releases/download/$TAG
    airlock_fetch "$_url/$_asset" "$_tmp/$_asset"
    airlock_fetch "$_url/SHA256SUMS.txt" "$_tmp/SHA256SUMS.txt"
    _want=$(awk -v f="$_asset" '$2 == f { print $1 }' "$_tmp/SHA256SUMS.txt")
    _got=$(sha256sum "$_tmp/$_asset" | cut -d ' ' -f 1)
    [ -n "$_want" ] && [ "$_got" = "$_want" ] ||
        airlock_fail 12 "checksum mismatch: $_asset (want ${_want:-none}, got $_got)"

    airlock_log "installing Copilot CLI $TAG"
    if [ -z "$_node" ]; then
        # The archive member is owned by uid 1001. Thus write a new file.
        mkdir -p "${_real%/*}"
        { tar -xzOf "$_tmp/$_asset" copilot >"$_real.new" &&
            chmod 755 "$_real.new" && mv -f "$_real.new" "$_real"; } ||
            airlock_fail 12 "could not unpack $_asset"
    else
        rm -rf "$_lib.new"
        mkdir -p "$_lib.new"
        tar -xzof "$_tmp/$_asset" -C "$_lib.new" --strip-components 1 &&
            [ -f "$_lib.new/index.js" ] ||
            airlock_fail 12 "could not unpack $_asset"
        rm -rf "$_lib"
        mv "$_lib.new" "$_lib"
    fi
    rm -f "$_tmp/$_asset"
    _check || airlock_fail 12 "copilot --help failed after the install"
fi
