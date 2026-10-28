# airlock packs: shared helpers for the setup scripts (`setup.sh`) of the
# built-in packs.
#
# The host runs one exec per pack, as root, in an install boot with an
# open network. The script is a wrapper (`exec 3>&1 1>&2`), this file,
# then the `setup.sh` of the pack. All output goes to stderr (the install
# log). Fd 3 carries the status protocol v1 (`AIRLOCK_PACK_API=1`). Only
# `airlock_steps` (one line `steps <n>`) and `airlock_status` (one line
# `status <text>` per step) write to it.
#
# Each external command that a helper here runs gets `3>&-`, except
# basic file and text tools (`rm`, `mkdir`, `chmod`, `sed`, ...). Thus no
# package manager or vendor installer can write status lines. The helpers
# write only to the log. They never add a step.
#
# Args come only from the environment: `AIRLOCK_PACK_ARG_<KEY>` (key in
# upper case, `-` changed to `_`). A bool arg is `true` or `false`.
#
# Each pack script is idempotent. A new run after a complete, interrupted
# or failed run does only the work that is still missing.
#
# Exit codes:
#  10: unsupported distro or architecture
#  11: package install failed
#  12: download or vendor installer failed
#  13: an arg value that the script cannot use (the `config.lua` of the
#      pack refuses it first)
#
# Public API (stable). All pack scripts can use these names:
#  - functions: airlock_steps, airlock_status, airlock_log, airlock_fail,
#    airlock_detect_distro, airlock_detect_arch, airlock_detect_libc,
#    airlock_pkg_is_installed, airlock_pkg_available, airlock_pkg_install,
#    airlock_fetch
#  - variables: DISTRO, ARCH, LIBC, AIRLOCK_PACK_TMPDIR
# Functions that start with `airlock_internal__`, and all names that
# start with `_`, are internal. They can change at any time. Only the
# built-in packs can use them. Other packs must not use them, and must
# not define names that start with `airlock_` or `_airlock_`.

set -eu
umask 022

export HOME="${HOME:-/root}"
export DEBIAN_FRONTEND=noninteractive
# The exec can have a minimal PATH. The packs install into these folders.
export PATH="${PATH:+$PATH:}/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
# airlock merges its CA into the system bundle. Point each TLS stack at
# it. Static binaries possibly do not know the bundle path of the distro.
# Bun and the Node.js builds of nodejs.org read only their own CA list and
# NODE_EXTRA_CA_CERTS.
if [ -f /etc/ssl/certs/ca-certificates.crt ]; then
    export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
    export NODE_EXTRA_CA_CERTS=/etc/ssl/certs/ca-certificates.crt
fi

# Test seams: another os-release file, and no apk/apt-get probe.
AIRLOCK_OS_RELEASE="${AIRLOCK_OS_RELEASE:-/etc/os-release}"
AIRLOCK_PKG_PROBE="${AIRLOCK_PKG_PROBE:-1}"

airlock_log() {
    printf 'airlock-pack: %s\n' "$*" >&2
}

# airlock_fail <exit code> <message>
airlock_fail() {
    _fail_code=$1
    shift
    airlock_log "error: $*"
    exit "$_fail_code"
}

# airlock_internal__send <line>: write <line> to the status channel (fd 3), only
# under airlock (AIRLOCK_PACK_API=1).
airlock_internal__send() {
    if [ "${AIRLOCK_PACK_API:-}" = 1 ]; then
        { printf '%s\n' "$1" >&3; } 2>/dev/null || :
    fi
}

# airlock_steps <n>: the script has <n> steps (1 to 99), one
# airlock_status each. Call it once, before the first step. The host then
# shows "[<i>/<n>]" after the status of step <i>. Send the status of each
# step also when its work is already done. Otherwise the numbers are wrong.
airlock_steps() {
    airlock_internal__send "steps $1"
}

# airlock_status <text>: show <text> next to the pack label on the host,
# as the next step after airlock_steps. Outside airlock (no
# AIRLOCK_PACK_API), the text goes to the log only.
airlock_status() {
    airlock_internal__send "status $*"
    airlock_log "$*"
}

# Print "alpine" or "debian", from os-release (ID, then ID_LIKE), else
# from the package manager on PATH. Return 1 when neither matches.
airlock_detect_distro() {
    _dd_ids=
    if [ -r "$AIRLOCK_OS_RELEASE" ]; then
        _dd_ids=$(
            set +eu
            # shellcheck disable=SC1090 # a test seam picks the file
            . "$AIRLOCK_OS_RELEASE" >/dev/null 2>&1 3>&-
            printf ' %s %s ' "${ID:-}" "${ID_LIKE:-}"
        ) || _dd_ids=
    fi
    case "$_dd_ids" in
        *' alpine '*)
            echo alpine
            return 0
            ;;
        *' debian '* | *' ubuntu '*)
            echo debian
            return 0
            ;;
    esac
    if [ "$AIRLOCK_PKG_PROBE" = 1 ]; then
        if command -v apk >/dev/null 2>&1; then
            echo alpine
            return 0
        fi
        if command -v apt-get >/dev/null 2>&1; then
            echo debian
            return 0
        fi
    fi
    return 1
}

# Print "x86_64" or "aarch64". Return 1 for other machines.
airlock_detect_arch() {
    case "$(uname -m 3>&-)" in
        x86_64 | amd64) echo x86_64 ;;
        aarch64 | arm64) echo aarch64 ;;
        *) return 1 ;;
    esac
}

# Print "musl" or "glibc". The getconf of glibc knows GNU_LIBC_VERSION.
# A glibc system can also have the musl loader (the musl package). Thus
# the loader on disk is only the fallback.
airlock_detect_libc() {
    if getconf GNU_LIBC_VERSION >/dev/null 2>&1 3>&-; then
        echo glibc
        return 0
    fi
    for _dl_f in /lib/ld-musl-*.so.1; do
        if [ -e "$_dl_f" ]; then
            echo musl
            return 0
        fi
    done
    echo glibc
}

_airlock_apt_ready=0
_airlock_apt_updated=0

# Debian family, once per run, before the first apt-get install.
airlock_internal__apt_prepare() {
    if [ "$_airlock_apt_ready" = 1 ]; then
        return 0
    fi
    # No init system runs in the sandbox. Stop package scripts from
    # starting services. Debian images have this file, custom images
    # possibly do not.
    if [ ! -e /usr/sbin/policy-rc.d ]; then
        printf '#!/bin/sh\nexit 101\n' >/usr/sbin/policy-rc.d
        chmod 755 /usr/sbin/policy-rc.d
    fi
    airlock_internal__dpkg_repair
    _airlock_apt_ready=1
}

# An interrupted earlier run can leave dpkg half-done. Then each later
# apt-get install fails. Thus finish that work first. Otherwise this
# changes nothing.
airlock_internal__dpkg_repair() {
    dpkg --configure -a 3>&- || airlock_fail 11 "dpkg --configure -a failed"
    if ! apt-get -f install -y -q --no-install-recommends 3>&-; then
        # The fix can need packages that the index does not have (an old
        # or empty index).
        airlock_internal__apt_update_once
        apt-get -f install -y -q --no-install-recommends 3>&- ||
            airlock_fail 11 "apt-get -f install failed"
    fi
}

airlock_internal__apt_update_once() {
    if [ "$_airlock_apt_updated" = 0 ]; then
        apt-get update -q 3>&- || airlock_fail 12 "apt-get update failed"
        _airlock_apt_updated=1
    fi
}

airlock_pkg_is_installed() {
    case "$DISTRO" in
        alpine) apk info -e "$1" >/dev/null 2>&1 3>&- ;;
        debian) [ "$(dpkg-query -W -f='${Status}' "$1" 2>/dev/null 3>&-)" = "install ok installed" ] ;;
        *) return 1 ;;
    esac
}

# Return 0 if the package index has the package. Only Debian checks the
# index. Alpine always returns 0.
airlock_pkg_available() {
    case "$DISTRO" in
        debian)
            airlock_internal__apt_prepare
            airlock_internal__apt_update_once
            apt-cache policy "$1" 2>/dev/null 3>&- |
                sed -n 's/^ *Candidate: *//p' | grep -qv '(none)'
            ;;
        *) return 0 ;;
    esac
}

# Install the packages that are not installed yet.
airlock_pkg_install() {
    _pi_missing=
    for _pi_p in "$@"; do
        airlock_pkg_is_installed "$_pi_p" || _pi_missing="$_pi_missing $_pi_p"
    done
    if [ -z "$_pi_missing" ]; then
        return 0
    fi
    airlock_log "installing packages:$_pi_missing"
    case "$DISTRO" in
        alpine)
            # shellcheck disable=SC2086 # word splitting is intended
            apk add --no-cache $_pi_missing 3>&- ||
                airlock_fail 11 "apk add failed:$_pi_missing"
            ;;
        debian)
            airlock_internal__apt_prepare
            airlock_internal__apt_update_once
            # shellcheck disable=SC2086 # word splitting is intended
            apt-get install -y -q --no-install-recommends $_pi_missing 3>&- ||
                airlock_fail 11 "apt-get install failed:$_pi_missing"
            ;;
    esac
}

# airlock_fetch <url> <file>: download over HTTPS. Installs curl first when it
# is missing.
airlock_fetch() {
    if ! command -v curl >/dev/null 2>&1; then
        airlock_pkg_install curl ca-certificates
    fi
    curl -fsSL --retry 3 --proto '=https' --tlsv1.2 -o "$2" "$1" 3>&- ||
        airlock_fail 12 "download failed: $1"
}

# airlock_internal__run_vendor <name> <command> [args...]: run an upstream
# installer or tool, without fd 3 and with stdin closed. A failure is
# exit 12.
airlock_internal__run_vendor() {
    _rv_name=$1
    shift
    "$@" </dev/null 3>&- || airlock_fail 12 "$_rv_name failed"
}

# airlock_internal__github_latest_tag <owner>/<repo>: set TAG to the tag of
# the latest release, the one that
# https://github.com/<owner>/<repo>/releases/latest redirects to. Installs
# curl first when it is missing.
airlock_internal__github_latest_tag() {
    if ! command -v curl >/dev/null 2>&1; then
        airlock_pkg_install curl ca-certificates
    fi
    TAG=$(curl -fsSLI --retry 3 --proto '=https' --tlsv1.2 -o /dev/null \
        -w '%{url_effective}' "https://github.com/$1/releases/latest" 3>&-) ||
        airlock_fail 12 "could not find the latest release of $1"
    TAG=${TAG##*/}
    case "$TAG" in
        '' | latest | releases) airlock_fail 12 "$1 has no latest release" ;;
    esac
}

# airlock_internal__bun_cleanup: remove Bun and everything that
# airlock_internal__bun_get and airlock_internal__bun_compile put in
# $AIRLOCK_PACK_TMPDIR, and unset BUN.
airlock_internal__bun_cleanup() {
    rm -rf "$AIRLOCK_PACK_TMPDIR/bun" "$AIRLOCK_PACK_TMPDIR/bun-home" \
        "$AIRLOCK_PACK_TMPDIR/bun-cache" "$AIRLOCK_PACK_TMPDIR"/bun-build-* \
        "$AIRLOCK_PACK_TMPDIR"/bun-linux-*.zip \
        "$AIRLOCK_PACK_TMPDIR/bun-SHASUMS256.txt"
    unset BUN
}

# airlock_internal__bun_fail <exit code> <message>: fail after
# airlock_internal__bun_cleanup.
airlock_internal__bun_fail() {
    airlock_internal__bun_cleanup
    airlock_fail "$@"
}

# airlock_internal__bun_get: set BUN to a Bun from the latest release of
# oven-sh/bun, in $AIRLOCK_PACK_TMPDIR/bun. airlock_internal__bun_compile
# removes it again (airlock_internal__bun_cleanup). Downloads the zip for
# $ARCH and $LIBC and checks it against the SHASUMS256.txt of the release.
# On x86_64, use the baseline build. It runs also on CPUs without AVX2, and
# `bun build --compile` copies the running Bun into each executable.
# Sources: https://bun.com/docs/installation (the release zips, Alpine
# needs libgcc and libstdc++) and https://github.com/oven-sh/bun/releases
# (bun-linux-<x64-baseline|x64-musl-baseline|aarch64|aarch64-musl>.zip
# with one file bun-linux-<...>/bun, and SHASUMS256.txt).
airlock_internal__bun_get() {
    case "$ARCH" in
        x86_64) _bg_arch=x64 ;;
        aarch64) _bg_arch=aarch64 ;;
    esac
    _bg_name=bun-linux-$_bg_arch
    if [ "$LIBC" = musl ]; then
        _bg_name=$_bg_name-musl
    fi
    if [ "$ARCH" = x86_64 ]; then
        _bg_name=$_bg_name-baseline
    fi
    # unzip: the release is a zip. The musl build links libstdc++ (with
    # libgcc). Each executable that Bun compiles there also links it.
    case "$DISTRO" in
        alpine) airlock_pkg_install ca-certificates curl unzip libgcc libstdc++ ;;
        debian) airlock_pkg_install ca-certificates curl unzip ;;
    esac
    # One tag for the zip and SHASUMS256.txt.
    airlock_internal__github_latest_tag oven-sh/bun
    case "$TAG" in
        bun-v[0-9]*) ;;
        *) airlock_fail 12 "unexpected latest Bun release: $TAG" ;;
    esac
    airlock_log "downloading Bun ${TAG#bun-}"
    _bg_url=https://github.com/oven-sh/bun/releases/download/$TAG
    airlock_fetch "$_bg_url/$_bg_name.zip" "$AIRLOCK_PACK_TMPDIR/$_bg_name.zip"
    airlock_fetch "$_bg_url/SHASUMS256.txt" \
        "$AIRLOCK_PACK_TMPDIR/bun-SHASUMS256.txt"
    _bg_want=$(awk -v f="$_bg_name.zip" '$2 == f { print $1 }' \
        "$AIRLOCK_PACK_TMPDIR/bun-SHASUMS256.txt" 3>&-)
    _bg_got=$(sha256sum "$AIRLOCK_PACK_TMPDIR/$_bg_name.zip" 3>&-) ||
        airlock_internal__bun_fail 12 "sha256sum failed: $_bg_name.zip"
    _bg_got=${_bg_got%% *}
    if [ -z "$_bg_want" ] || [ "$_bg_got" != "$_bg_want" ]; then
        airlock_internal__bun_fail 12 \
            "checksum mismatch: $_bg_name.zip (want ${_bg_want:-none}, got $_bg_got)"
    fi
    rm -rf "$AIRLOCK_PACK_TMPDIR/bun"
    mkdir -p "$AIRLOCK_PACK_TMPDIR/bun"
    unzip -q -o "$AIRLOCK_PACK_TMPDIR/$_bg_name.zip" "$_bg_name/bun" \
        -d "$AIRLOCK_PACK_TMPDIR/bun" </dev/null 3>&- ||
        airlock_internal__bun_fail 12 "could not unpack $_bg_name.zip"
    rm -f "$AIRLOCK_PACK_TMPDIR/$_bg_name.zip" \
        "$AIRLOCK_PACK_TMPDIR/bun-SHASUMS256.txt"
    BUN=$AIRLOCK_PACK_TMPDIR/bun/$_bg_name/bun
    chmod 755 "$BUN"
    "$BUN" --version </dev/null >/dev/null 2>&1 3>&- ||
        airlock_internal__bun_fail 12 "bun --version failed: $BUN"
}

# airlock_internal__bun_compile <package> <command> <file>: compile the
# command <command> of the npm package <package> (its package.json `bin`)
# into <file>. The result is one root-owned executable (0755) with the Bun
# runtime and the bundled JavaScript. `bun add` installs the package into
# a scratch project, without optional dependencies (the platform binaries
# of a package are not in the executable) and without install scripts. Each
# call gets its own Bun (airlock_internal__bun_get). Also on failure, each
# call removes Bun, the scratch project, its node_modules and the Bun cache
# (airlock_internal__bun_cleanup). Only <file> stays in the image. Skips
# the build when `<file> --version` exits 0 and <file> is not a script. A
# script, for example an `airlock_internal__acp_stub` stub, is always
# replaced.
# Source: https://bun.com/docs/bundler/executables (`bun build
# --compile`, the embedded entry is process.argv[1], under /$bunfs/root).
airlock_internal__bun_compile() {
    if "$3" --version </dev/null >/dev/null 2>&1 3>&- &&
        [ "$(head -c 2 "$3" 3>&-)" != '#!' ]; then
        airlock_log "$2 is already installed"
        return 0
    fi
    airlock_internal__bun_get
    _bc_dir=$AIRLOCK_PACK_TMPDIR/bun-build-$2
    rm -rf "$_bc_dir"
    mkdir -p "$_bc_dir"
    printf '{"private":true}\n' >"$_bc_dir/package.json"
    airlock_log "installing $1"
    env BUN_INSTALL="$AIRLOCK_PACK_TMPDIR/bun-home" \
        BUN_INSTALL_CACHE_DIR="$AIRLOCK_PACK_TMPDIR/bun-cache" \
        DO_NOT_TRACK=1 "$BUN" add --cwd "$_bc_dir" --omit=optional \
        --ignore-scripts --no-progress "$1" </dev/null 3>&- ||
        airlock_internal__bun_fail 12 "bun add $1 failed"
    # The entry imports the bin file of <command>. In a Bun executable,
    # process.argv[1] is the embedded entry path. A Node.js program that
    # runs itself again as process.execPath with process.argv.slice(1)
    # thus gives that path once more as argv[2]. The entry removes it
    # before the package code reads the args.
    # shellcheck disable=SC2016 # JavaScript, not shell
    DO_NOT_TRACK=1 "$BUN" -e '
        const fs = require("fs");
        const path = require("path");
        const [dir, pkg, cmd] = process.argv.slice(1);
        const root = path.join(dir, "node_modules", pkg);
        const meta = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
        const bin = typeof meta.bin === "string"
            ? (meta.name.split("/").pop() === cmd ? meta.bin : undefined)
            : (meta.bin || {})[cmd];
        if (!bin || !fs.statSync(path.join(root, bin), { throwIfNoEntry: false })?.isFile()) {
            process.exit(1);
        }
        fs.writeFileSync(path.join(dir, "airlock-entry.mjs"),
            "if (process.argv[2] === process.argv[1]) process.argv.splice(2, 1);\n" +
            "await import(" + JSON.stringify(path.join(root, bin)) + ");\n");
    ' "$_bc_dir" "$1" "$2" </dev/null 3>&- ||
        airlock_internal__bun_fail 12 "$1 has no command $2"
    airlock_log "compiling $2"
    DO_NOT_TRACK=1 "$BUN" build --compile --outfile "$_bc_dir/$2" \
        "$_bc_dir/airlock-entry.mjs" </dev/null 3>&- ||
        airlock_internal__bun_fail 12 "bun build $1 failed"
    # Write next to <file>, then rename. Thus <file> is never half
    # written.
    if ! { mkdir -p "${3%/*}" && rm -f "$3.new" && mv -f "$_bc_dir/$2" "$3.new" &&
        chown 0:0 "$3.new" && chmod 755 "$3.new" && mv -f "$3.new" "$3"; }; then
        rm -f "$3.new"
        airlock_internal__bun_fail 12 "could not install $3"
    fi
    airlock_internal__bun_cleanup
}

# airlock_internal__acp_stub <file> <agent>: write to <file> a script that
# writes "airlock acp support for <agent> is not enabled" to stderr and
# exits 1. An agent pack puts it at its ACP adapter path when its `acp` arg
# is off. Clients that start the adapter get a clear error, not "command
# not found".
airlock_internal__acp_stub() {
    mkdir -p "${1%/*}"
    cat >"$1.new" <<STUB
#!/bin/sh
# Managed by airlock: the pack arg \`acp\` is off.
echo 'airlock acp support for $2 is not enabled' >&2
exit 1
STUB
    chmod 755 "$1.new"
    mv -f "$1.new" "$1"
}

DISTRO=$(airlock_detect_distro) ||
    airlock_fail 10 "unsupported image: not Alpine- or Debian-based (no apk or apt-get)"
ARCH=$(airlock_detect_arch) || airlock_fail 10 "unsupported architecture: $(uname -m 3>&-)"
LIBC=$(airlock_detect_libc)
export DISTRO ARCH LIBC

# Scratch space for downloads. The EXIT trap removes it. It is on the
# sandbox disk, not in /tmp (a tmpfs in memory), because some downloads
# are hundreds of MB. TMPDIR points there too, for the vendor installers.
# Packs install one at a time. Thus each other airlock-pack.* directory
# is left from an interrupted run.
_pack_tmp_root=${TMPDIR:-/var/tmp}
rm -rf "$_pack_tmp_root"/airlock-pack.*
mkdir -p "$_pack_tmp_root"
AIRLOCK_PACK_TMPDIR=$(mktemp -d "$_pack_tmp_root/airlock-pack.XXXXXX")
TMPDIR=$AIRLOCK_PACK_TMPDIR
export TMPDIR
trap 'rm -rf "$AIRLOCK_PACK_TMPDIR"' EXIT
cd /
airlock_log "distribution family: $DISTRO, architecture: $ARCH, libc: $LIBC"
