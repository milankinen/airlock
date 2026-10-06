# mise: the official installer (https://mise.run) puts the mise binary in
# /usr/local/bin; a profile.d file puts the mise shims on the login-shell
# PATH of every user.
#
# Sources:
# - https://mise.jdx.dev/installing-mise.html (`curl -fsSL https://mise.run
#   | sh`; MISE_INSTALL_PATH, MISE_INSTALL_MUSL; the static -musl builds;
#   "newest stable release published at least 24 hours ago")
# - https://github.com/jdx/mise/blob/main/packaging/standalone/install.envsubst
#   (the script that https://mise.run serves; MISE_INSTALL_HELP; musl
#   detection through `ldd`; root installs get chown 0:0 + chmod 755)
# - https://mise.jdx.dev/cli/activate.html and
#   https://mise.jdx.dev/dev-tools/shims.html (`mise activate --shims` is
#   PATH="<shims>:$PATH"; there is no `mise activate sh`)
# - https://mise.jdx.dev/directories.html (MISE_DATA_DIR, XDG_DATA_HOME,
#   MISE_SHIMS_DIR)
#
# No sha pin: the installer checks the release's SHASUMS256.txt.
# No args.

_mise=/usr/local/bin/mise

airlock_steps 1
airlock_status "installing mise"
if _have=$("$_mise" version </dev/null 2>/dev/null 3>&-); then
    log "mise is already installed: $_have"
else
    # ca-certificates: mise downloads tools over HTTPS. musl-utils: the
    # installer picks the -musl build only when `ldd` exists. xz-utils:
    # many tool archives that mise downloads are .tar.xz.
    case "$DISTRO" in
        alpine) pkg_install ca-certificates musl-utils ;;
        debian) pkg_install ca-certificates xz-utils ;;
    esac
    log "downloading the mise installer"
    fetch https://mise.run "$PACK_TMP/mise-install.sh"
    if [ "$LIBC" = musl ]; then
        _musl=1
    else
        _musl=0
    fi
    # The installer's own downloads (release index, ~50 MB tarball,
    # SHASUMS256.txt) have no retry: try up to 3 times.
    _try=1
    while :; do
        # An executable that does not run (an interrupted copy) stops an
        # unpinned installer: "cannot compare installed mise version".
        rm -f "$_mise"
        # The installer leaves its scratch directories behind when it
        # fails; one clean TMPDIR per attempt.
        rm -rf "$PACK_TMP/mise"
        mkdir "$PACK_TMP/mise"
        if env MISE_INSTALL_PATH="$_mise" MISE_INSTALL_MUSL="$_musl" \
            MISE_INSTALL_HELP=0 TMPDIR="$PACK_TMP/mise" \
            sh "$PACK_TMP/mise-install.sh" </dev/null 3>&-; then
            break
        fi
        if [ "$_try" -ge 3 ]; then
            fail 12 "the mise installer failed"
        fi
        _try=$((_try + 1))
        log "installing mise (attempt $_try of 3)"
        sleep 3
    done
    rm -rf "$PACK_TMP/mise"
fi

# /etc/profile resets PATH in login shells; put the mise shims back. The
# shims directory follows mise's own lookup: MISE_SHIMS_DIR, else
# MISE_DATA_DIR/shims, else $XDG_DATA_HOME/mise/shims, else
# ~/.local/share/mise/shims.
mkdir -p /etc/profile.d
cat >/etc/profile.d/airlock-mise.sh <<'PROFILE'
# Managed by airlock: the mise shims for login shells.
_p="${MISE_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/mise}/shims"
_p="${MISE_SHIMS_DIR:-$_p}"
case ":$PATH:" in
    *":$_p:"*) ;;
    *) PATH="$_p:$PATH" ;;
esac
unset _p
export PATH
PROFILE

"$_mise" version </dev/null 3>&- || fail 12 "mise version failed"
