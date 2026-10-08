# GitHub Copilot CLI: the official release build from GitHub, for all
# users on PATH (/usr/local/bin/copilot). ACP is built in
# (`copilot --acp`, stdio by default).
#
# Sources:
# - https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/install-copilot-cli
#   (npm with Node.js 22+, Homebrew, the install script, and "Download
#   from GitHub.com": download the executable, unpack it, run it)
# - https://gh.io/copilot-install -> https://raw.githubusercontent.com/github/copilot-cli/refs/heads/main/install.sh
#   (bash. It always takes copilot-linux-<x64|arm64>.tar.gz, also on
#   musl, and its `sha256sum -c --ignore-missing` fails under BusyBox.
#   This script does the same download and check, with the musl build on
#   musl.)
# - https://github.com/github/copilot-cli/releases (copilot-<linux|linuxmusl>-<x64|arm64>.tar.gz
#   with one file `copilot` owned by uid 1001, github-copilot-<ver>-<plat>-<arch>.tgz
#   packages, SHA256SUMS.txt)
# - https://github.com/github/copilot-cli/blob/main/changelog.md (1.0.49:
#   Alpine/musl support, 0.0.397: `--acp`)
# - https://docs.github.com/en/copilot/reference/copilot-cli-reference/acp-server
#
# musl + aarch64: the standalone linuxmusl-arm64 executable (checked:
# 1.0.64, 1.0.86, 1.0.91) cannot load its native addons on Alpine
# ("Node-API symbol ... has not been loaded"). Only `--version` works.
# Thus on that platform the linuxmusl-arm64 package of the same release
# runs on the Node.js of Alpine.
#
# No sha pin: the script checks each download against the SHA256SUMS.txt
# of the release, as the install script does.
# No args.

_bin=/usr/local/bin/copilot
_lib=/usr/local/lib/copilot
_repo=github/copilot-cli

case "$ARCH" in
    x86_64) _arch=x64 ;;
    aarch64) _arch=arm64 ;;
esac
if [ "$LIBC" = musl ]; then
    _plat=linuxmusl
else
    _plat=linux
fi
_use_node=0
if [ "$_plat" = linuxmusl ] && [ "$_arch" = arm64 ]; then
    _use_node=1
fi

# `copilot --help` loads the native runtime addon (`--version` does
# not). The run as root unpacks the package (about 180 MB) into a cache.
# Keep that cache and all state in the scratch directory. No auto-update.
copilot_works() {
    mkdir -p "$PACK_TMP/home"
    HOME="$PACK_TMP/home" XDG_CACHE_HOME="$PACK_TMP/cache" COPILOT_AUTO_UPDATE=false \
        "$_bin" --help </dev/null >/dev/null 2>&1 3>&-
}

# fetch_release <tag> <asset>: download a release asset to $PACK_TMP and
# check it against the SHA256SUMS.txt of the release (as install.sh
# does).
fetch_release() {
    fetch "https://github.com/$_repo/releases/download/$1/$2" "$PACK_TMP/$2"
    if [ ! -f "$PACK_TMP/SHA256SUMS.txt" ]; then
        fetch "https://github.com/$_repo/releases/download/$1/SHA256SUMS.txt" \
            "$PACK_TMP/SHA256SUMS.txt"
    fi
    _fr_want=$(awk -v f="$2" '$2 == f { print $1 }' "$PACK_TMP/SHA256SUMS.txt" 3>&-)
    _fr_got=$(sha256sum "$PACK_TMP/$2" 3>&-) || fail 12 "sha256sum failed: $2"
    _fr_got=${_fr_got%% *}
    if [ -z "$_fr_want" ] || [ "$_fr_got" != "$_fr_want" ]; then
        rm -f "$PACK_TMP/$2"
        fail 12 "checksum mismatch: $2 (want ${_fr_want:-none}, got $_fr_got)"
    fi
}

airlock_steps 1
airlock_status "installing cli"
# bash: the Copilot shell tool (it looks for /bin/bash, /usr/bin/bash,
# /usr/local/bin/bash). libstdc++/libgcc: the glibc build links them.
# nodejs: runs the package on musl aarch64 (Alpine has Node.js 22+).
case "$DISTRO" in
    alpine)
        if [ "$_use_node" = 1 ]; then
            pkg_install bash ca-certificates curl nodejs
        else
            pkg_install bash ca-certificates curl
        fi
        ;;
    debian) pkg_install bash ca-certificates curl libstdc++6 libgcc-s1 ;;
esac

if [ -x "$_bin" ] && copilot_works; then
    log "Copilot CLI is already installed"
else
    # One tag for the asset and SHA256SUMS.txt.
    github_latest_tag "$_repo"
    case "$TAG" in
        v[0-9]*) ;;
        *) fail 12 "unexpected latest Copilot CLI release: $TAG" ;;
    esac
    mkdir -p /usr/local/bin /usr/local/lib
    rm -f "$_bin.new"
    if [ "$_use_node" = 1 ]; then
        _asset="github-copilot-${TAG#v}-$_plat-$_arch.tgz"
        log "downloading Copilot CLI $TAG"
        fetch_release "$TAG" "$_asset"
        log "installing Copilot CLI $TAG"
        rm -rf "$_lib.new"
        mkdir -p "$_lib.new"
        tar -xzof "$PACK_TMP/$_asset" -C "$_lib.new" --strip-components 1 3>&- ||
            fail 12 "could not unpack $_asset"
        rm -f "$PACK_TMP/$_asset"
        [ -f "$_lib.new/index.js" ] || fail 12 "$_asset has no index.js"
        rm -rf "$_lib"
        mv "$_lib.new" "$_lib"
        cat >"$_bin.new" <<WRAPPER
#!/bin/sh
# Managed by airlock: the Copilot CLI package on the distro Node.js (the
# standalone linuxmusl-arm64 executable cannot load its native addons).
exec /usr/bin/node $_lib/index.js "\$@"
WRAPPER
    else
        _asset="copilot-$_plat-$_arch.tar.gz"
        log "downloading Copilot CLI $TAG"
        fetch_release "$TAG" "$_asset"
        log "installing Copilot CLI $TAG"
        # The archive has one member, `copilot`, owned by uid 1001. tar as
        # root keeps that owner. Thus write a new root-owned file.
        if ! tar -xzOf "$PACK_TMP/$_asset" copilot >"$_bin.new" 3>&-; then
            rm -f "$_bin.new"
            fail 12 "could not unpack $_asset"
        fi
        rm -f "$PACK_TMP/$_asset"
    fi
    chmod 755 "$_bin.new"
    mv -f "$_bin.new" "$_bin"
    copilot_works || fail 12 "copilot --help failed after the install"
fi

HOME="$PACK_TMP/home" XDG_CACHE_HOME="$PACK_TMP/cache" COPILOT_AUTO_UPDATE=false \
    "$_bin" --version </dev/null 3>&- || fail 12 "copilot --version failed"
