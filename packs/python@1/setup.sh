# Python through uv: the official uv installer puts uv and uvx in
# /usr/local/bin; `uv python install` puts a python-build-standalone
# CPython in /usr/local/lib/uv/python and links python3.<minor>,
# python3 and python into /usr/local/bin (on every user's PATH).
#
# Sources:
# - https://docs.astral.sh/uv/getting-started/installation/ (the
#   official installer https://astral.sh/uv/install.sh)
# - https://docs.astral.sh/uv/reference/installer/ (UV_UNMANAGED_INSTALL:
#   install dir, no shell profile changes, no `uv self update`)
# - https://docs.astral.sh/uv/concepts/python-versions/ (request
#   formats such as `3`, `3.12`, `3.13t`, `pypy@3.11`; uv does not
#   replace executables that it does not manage)
# - https://docs.astral.sh/uv/reference/cli/#uv-python-install
#   (`--default`: also python3 and python; `--compile-bytecode`)
# - https://docs.astral.sh/uv/reference/environment/ (UV_PYTHON_INSTALL_DIR,
#   UV_PYTHON_BIN_DIR, UV_CACHE_DIR, UV_PYTHON_DOWNLOADS)
# - https://docs.astral.sh/uv/concepts/preview/ (python-install-default;
#   an unknown preview feature name only warns)
# - https://docs.astral.sh/uv/concepts/authentication/certificates/
#   (uv trusts only SSL_CERT_FILE when it is set; lib.sh exports it)
# - https://gregoryszorc.com/docs/python-build-standalone/main/running.html
#   (dynamically linked musl builds for Alpine)
#
# No sha pin: the installer checks the sha256 of the uv it downloads.
# Args: python-version (AIRLOCK_PACK_ARG_PYTHON_VERSION): latest, none,
# or a uv Python request (3.13, 3.12, 3.12.4, 3.13t, pypy@3.11, ...).
# "latest" is the request `3`: the newest stable CPython 3 that this uv
# release knows. pypi-installs changes only the network rules
# (config.lua).

_uv=/usr/local/bin/uv
_bin_dir=/usr/local/bin
_py_dir=/usr/local/lib/uv/python
_version=${AIRLOCK_PACK_ARG_PYTHON_VERSION:-latest}
case "$_version" in
    -* | *[!0-9A-Za-z.@+_-]*) fail 13 "invalid python-version: $_version" ;;
esac

# The config env also reaches this script. These variables would move
# uv out of /usr/local/bin or stop `uv python install`: ignore them here.
unset UV_INSTALL_DIR CARGO_DIST_FORCE_INSTALL_DIR UV_PYTHON_DOWNLOADS \
    UV_OFFLINE UV_NO_MANAGED_PYTHON UV_PYTHON_PREFERENCE

# Shared, root-owned Python installs; links on the default PATH; no
# cache left in /root.
UV_PYTHON_INSTALL_DIR=$_py_dir
UV_PYTHON_BIN_DIR=$_bin_dir
UV_CACHE_DIR=$PACK_TMP/uv-cache
export UV_PYTHON_INSTALL_DIR UV_PYTHON_BIN_DIR UV_CACHE_DIR

# Steps: uv, then Python unless python-version is none.
if [ "$_version" = none ]; then
    airlock_steps 1
else
    airlock_steps 2
fi

airlock_status "installing uv"
if [ -x "$_uv" ] && [ -x "$_bin_dir/uvx" ]; then
    log "uv is already installed"
else
    log "downloading the uv installer"
    fetch https://astral.sh/uv/install.sh "$PACK_TMP/uv-install.sh"
    mkdir -p "$_bin_dir"
    run_vendor "the uv installer" env UV_UNMANAGED_INSTALL="$_bin_dir" \
        sh "$PACK_TMP/uv-install.sh"
fi
"$_uv" --version </dev/null 3>&- || fail 12 "uv --version failed"

case "$_version" in
    none)
        log "python-version is none: no Python is installed"
        ;;
    *)
        case "$_version" in
            latest) _request=3 ;;
            *) _request=$_version ;;
        esac
        airlock_status "installing python $_version"
        # --default: python3 and python too (python3t and pythont for a
        # free-threaded build, pypy3 and pypy for PyPy). --compile-bytecode:
        # the directory is root-owned, so other users cannot write .pyc
        # files. `--`: a request that starts with `-` is not an option.
        run_vendor "uv python install $_request" "$_uv" python install \
            --no-progress --compile-bytecode \
            --default --preview-features python-install-default -- "$_request"
        _python=$("$_uv" python find --managed-python --no-python-downloads \
            -- "$_request" </dev/null 3>&-) ||
            fail 12 "uv python find $_request failed"
        "$_python" --version </dev/null 3>&- || fail 12 "$_python --version failed"

        # uv exits 0 when it does not replace an executable that it does
        # not manage (it only warns): check the links. From python3.14:
        # python3.14, python3, python (python3.13t: python3.13t, python3t,
        # pythont; pypy3.11: pypy3.11, pypy3, pypy).
        _name=${_python##*/}
        _major=$(printf '%s\n' "$_name" | sed 's/^\([a-z]*[0-9]*\)\.[0-9]*/\1/' 3>&-)
        _plain=$(printf '%s\n' "$_major" | sed 's/^\([a-z]*\)[0-9]*/\1/' 3>&-)
        for _link in "$_name" "$_major" "$_plain"; do
            if [ ! -e "$_bin_dir/$_link" ]; then
                fail 12 "uv did not install $_bin_dir/$_link"
            fi
            _target=$(readlink -f "$_bin_dir/$_link" 3>&-) || _target=
            case "$_target" in
                "$_py_dir"/*) ;;
                *) log "warning: $_bin_dir/$_link is not from uv; it is left in place" ;;
            esac
        done
        ;;
esac

# Login shells reset PATH in /etc/profile: add ~/.local/bin, where
# `uv tool install` and a user's own `uv python install` put commands.
mkdir -p /etc/profile.d
cat >/etc/profile.d/airlock-python.sh <<'PROFILE'
# Managed by airlock: commands from `uv tool install`.
case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) PATH="$HOME/.local/bin:$PATH" ;;
esac
export PATH
PROFILE
