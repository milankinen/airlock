# Node.js through nvm: nvm in /usr/local/nvm (one copy for all users),
# the chosen Node.js version as nvm's default, its commands on the default
# PATH through /usr/local/bin, and nvm itself in login shells (profile.d).
#
# Sources:
# - https://github.com/nvm-sh/nvm#install--update-script (the install
#   script of a release: https://raw.githubusercontent.com/nvm-sh/nvm/<tag>/install.sh;
#   NVM_DIR, PROFILE=/dev/null; `nvm install 'lts/*'`, `nvm install
#   node`; "Platforms without official binaries": NVM_NODEJS_ORG_MIRROR,
#   NVM_NO_SOURCE_FALLBACK; "Environment variables": install locks under
#   $NVM_DIR/.cache/locks)
# - https://github.com/nvm-sh/nvm/blob/master/install.sh (METHOD=script:
#   nvm.sh, nvm-exec, bash_completion, no git; a non-default NVM_DIR must
#   exist; NODE_VERSION makes it install Node.js)
# - https://github.com/nvm-sh/nvm/blob/master/.github/workflows/tests-alpine.yml
#   (Alpine binary install: packages, mirror, `nvm install -b`)
# - https://github.com/nodejs/unofficial-builds#readme (linux-x64-musl,
#   linux-arm64-musl; `apk add libstdc++`)
# - https://github.com/nodejs/node/blob/main/BUILDING.md (official Linux
#   binaries: glibc; Node.js 25 and later need the libatomic runtime)
#
# No sha pin: the latest nvm release; nvm checks each Node.js download
# against the mirror's SHASUMS256.txt.
# Args: node-version (AIRLOCK_PACK_ARG_NODE_VERSION): lts, latest, none,
# or any version that nvm understands (22, 22.11.0, lts/jod).
# npm-installs changes only the network rules (config.lua).

NVM_DIR=/usr/local/nvm
export NVM_DIR

_arg=${AIRLOCK_PACK_ARG_NODE_VERSION:-lts}
case "$_arg" in
    lts) _node='lts/*' ;;
    latest) _node=node ;;
    none) _node= ;;
    -* | *[!0-9A-Za-z./*_-]*) fail 13 "invalid node-version: $_arg" ;;
    *) _node=$_arg ;;
esac

# Steps: nvm, then Node.js unless node-version is none.
if [ -z "$_node" ]; then
    airlock_steps 1
else
    airlock_steps 2
fi

airlock_status "installing nvm"
case "$DISTRO" in
    alpine)
        # nvm asks for linux-<arch>-musl builds on Alpine; nodejs.org has
        # only some x64 ones, unofficial-builds has x64 and arm64.
        NVM_NODEJS_ORG_MIRROR=https://unofficial-builds.nodejs.org/download/release
        export NVM_NODEJS_ORG_MIRROR
        pkg_install bash ca-certificates curl tar xz gzip grep sed coreutils \
            libstdc++ libgcc
        ;;
    debian)
        pkg_install bash ca-certificates curl tar xz-utils gzip libatomic1
        ;;
esac

# nvm: install.sh is a bash script. Empty NODE_VERSION and NVM_SOURCE:
# an image ENV (node images set NODE_VERSION) must not change what it does.
if bash -c '. "$NVM_DIR/nvm.sh" --no-use && nvm --version' \
    </dev/null >/dev/null 2>&1 3>&-; then
    log "nvm is already installed"
else
    github_latest_tag nvm-sh/nvm
    log "installing nvm $TAG"
    fetch "https://raw.githubusercontent.com/nvm-sh/nvm/$TAG/install.sh" \
        "$PACK_TMP/nvm-install.sh"
    mkdir -p "$NVM_DIR"
    run_vendor "the nvm installer" env PROFILE=/dev/null METHOD=script \
        NVM_INSTALL_VERSION="$TAG" NVM_INSTALL_GITHUB_REPO= \
        NVM_SOURCE= NODE_VERSION= bash "$PACK_TMP/nvm-install.sh"
    [ -s "$NVM_DIR/nvm.sh" ] || fail 12 "the nvm installer did not create $NVM_DIR/nvm.sh"
fi

# Login shells: the nvm command, and the default version's bin directory
# (with the commands from `npm install -g`) on PATH. nvm.sh reads its
# arguments (--no-use, --install); source it from a function so that the
# arguments of the login shell's command do not get to it.
mkdir -p /etc/profile.d
{
    printf '# Managed by airlock: nvm and its default Node.js for login shells.\n'
    printf 'export NVM_DIR=%s\n' "$NVM_DIR"
    if [ -n "${NVM_NODEJS_ORG_MIRROR:-}" ]; then
        printf 'export NVM_NODEJS_ORG_MIRROR=%s\n' "$NVM_NODEJS_ORG_MIRROR"
    fi
    cat <<'PROFILE'
_airlock_nvm_load() {
    . "$NVM_DIR/nvm.sh"
}
if [ -s "$NVM_DIR/nvm.sh" ]; then
    _airlock_nvm_load
fi
unset -f _airlock_nvm_load
PROFILE
} >"$PACK_TMP/airlock-nvm.sh"
mv -f "$PACK_TMP/airlock-nvm.sh" /etc/profile.d/airlock-nvm.sh
chmod 644 /etc/profile.d/airlock-nvm.sh

if [ -z "$_node" ]; then
    log "node-version is none: nvm only, no Node.js"
else
    # -b: a missing binary is an error, not a long compile from source.
    # `nvm install` does nothing for an installed version. A killed
    # earlier run can leave an install lock, and nvm then waits 600 s and
    # fails; nothing else runs nvm during the install, so drop the locks.
    rm -rf "$NVM_DIR/.cache/locks"
    airlock_status "installing node $_arg"
    run_vendor "nvm install $_node" bash -c '
        unset PREFIX NPM_CONFIG_PREFIX npm_config_prefix
        . "$NVM_DIR/nvm.sh" --no-use || exit 1
        nvm install -b --no-progress --skip-default-packages "$1" || exit 1
        v=$(nvm version "$1") || exit 1
        nvm alias default "$v" || exit 1
        nvm cache clear
    ' airlock-nvm "$_node"

    _node_bin=$(bash -c '. "$NVM_DIR/nvm.sh" --no-use >/dev/null 2>&1 && nvm which default' \
        </dev/null 2>/dev/null 3>&-) || _node_bin=
    [ -x "$_node_bin" ] || fail 12 "nvm did not install Node.js ($_arg)"

    # node, npm, npx (corepack up to Node.js 24) on the default PATH of
    # every shell and user. Only links into $NVM_DIR and free names are
    # changed; other files are left alone.
    _bin_dir=${_node_bin%/node}
    mkdir -p /usr/local/bin
    for _link in /usr/local/bin/*; do
        [ -L "$_link" ] || continue
        case "$(readlink "$_link" 3>&-)" in
            "$_bin_dir"/*) ;;
            "$NVM_DIR"/*) rm -f "$_link" ;;
        esac
    done
    for _f in "$_bin_dir"/*; do
        [ -x "$_f" ] || continue
        _link="/usr/local/bin/${_f##*/}"
        if [ -L "$_link" ]; then
            case "$(readlink "$_link" 3>&-)" in
                "$NVM_DIR"/*) ln -sfn "$_f" "$_link" ;;
            esac
        elif [ ! -e "$_link" ]; then
            ln -s "$_f" "$_link"
        fi
    done

    /usr/local/bin/node --version </dev/null 3>&- || fail 12 "node --version failed"
    /usr/local/bin/npm --version </dev/null 3>&- || fail 12 "npm --version failed"
fi
