# Rust: rustup from the official installer script (https://sh.rustup.rs),
# with the toolchain from the `toolchain` arg (plus clippy and rustfmt,
# `none` installs only rustup), in a shared RUSTUP_HOME and CARGO_HOME
# under /usr/local (the layout of the official rust Docker images), plus
# a C toolchain for linking.
#
# Sources:
# - https://rust-lang.github.io/rustup/installation/other.html
#   (`curl ... https://sh.rustup.rs | sh`. rustup-init.sh downloads the
#   rustup-init for the platform and gives it its arguments.)
# - https://rust-lang.github.io/rustup/installation/index.html
#   (CARGO_HOME and RUSTUP_HOME must always be set and CARGO_HOME/bin
#   must be on PATH. Nightly in two steps: `--default-toolchain none -y`,
#   then `rustup toolchain install nightly --allow-downgrade
#   --profile minimal --component clippy`.)
# - https://rust-lang.github.io/rustup/concepts/toolchains.html
#   (<channel>[-<date>][-<host>]: stable, beta, nightly, 1.99, 1.99.0,
#   nightly-2026-09-01, ...)
# - https://rust-lang.github.io/rustup/concepts/profiles.html
# - https://github.com/rust-lang/docker-rust (Dockerfile-alpine.template,
#   Dockerfile-slim.template: RUSTUP_HOME=/usr/local/rustup,
#   CARGO_HOME=/usr/local/cargo, -y --no-modify-path --profile minimal
#   --default-host, then chmod -R a+w, gcc + musl-dev / libc6-dev)
#
# No sha pin: rustup checks the sha256 of each component it downloads.
# Args: toolchain (AIRLOCK_PACK_ARG_TOOLCHAIN): stable, beta, nightly,
# none, or any toolchain name of rustup. allow-cargo changes only the
# network rules (config.lua).
#
# The rustup proxies (cargo, rustc, ...) read RUSTUP_HOME on each call.
# Thus the run env must set RUSTUP_HOME and CARGO_HOME (config.lua `env`).
# /etc/profile.d sets them only for login shells.

RUSTUP_HOME=/usr/local/rustup
CARGO_HOME=/usr/local/cargo
export RUSTUP_HOME CARGO_HOME
_bin="$CARGO_HOME/bin"

_toolchain=${AIRLOCK_PACK_ARG_TOOLCHAIN:-stable}
case "$_toolchain" in
    -* | *[!A-Za-z0-9._-]*) fail 13 "invalid toolchain: $_toolchain" ;;
esac

case "$DISTRO" in
    alpine) _host="$ARCH-unknown-linux-musl" ;;
    *) _host="$ARCH-unknown-linux-gnu" ;;
esac

# Steps: rustup, then the toolchain unless toolchain is none.
if [ "$_toolchain" = none ]; then
    airlock_steps 1
else
    airlock_steps 2
fi

airlock_status "installing rustup"
# Linking needs a C compiler and the libc headers. Many crates also
# build C or C++ code. rustup-init.sh picks the musl or glibc build of
# rustup-init from `ldd --version`. On Alpine, ldd is in musl-utils
# (part of alpine:latest, installed here for slimmer Alpine images).
case "$DISTRO" in
    alpine) pkg_install build-base ca-certificates musl-utils ;;
    debian) pkg_install build-essential ca-certificates ;;
esac

if [ -x "$_bin/rustup" ]; then
    log "rustup is already installed"
else
    log "downloading rustup"
    fetch https://sh.rustup.rs "$PACK_TMP/rustup-init.sh"
    # -y: no prompt (stdin is closed). No toolchain here, because the next
    # step installs it, also after an interrupted earlier run. rustup
    # keeps `minimal` as the profile for toolchains that install later
    # (`rustup toolchain install`, a rust-toolchain.toml). They get no
    # clippy or rustfmt unless they ask for them (`-c clippy,rustfmt`, or
    # `components` in rust-toolchain.toml).
    run_vendor "rustup-init" sh "$PACK_TMP/rustup-init.sh" -y --no-modify-path \
        --default-host "$_host" --default-toolchain none --profile minimal
fi

# Always (idempotent): install the toolchain, or complete or update it,
# and make it the default. A nightly can lack clippy or rustfmt. Then
# rustup tries older nightlies (up to 21 days back). With
# --allow-downgrade, it also tries nightlies older than an installed
# nightly. A dated nightly (nightly-YYYY-MM-DD) gets no fallback.
if [ "$_toolchain" != none ]; then
    _downgrade=
    case "$_toolchain" in
        nightly*) _downgrade=--allow-downgrade ;;
    esac
    airlock_status "installing toolchain $_toolchain"
    # The profile `minimal` plus clippy and rustfmt is the `default`
    # profile without rust-docs. The components are large downloads.
    # rustup rolls back a failed install, thus try again (up to 3 times)
    # before the script fails.
    _try=1
    until "$_bin/rustup" toolchain install "$_toolchain" --profile minimal \
        --component clippy,rustfmt --no-self-update ${_downgrade:+"$_downgrade"} \
        </dev/null 3>&-; do
        [ "$_try" -lt 3 ] || fail 12 "rustup toolchain install $_toolchain failed"
        _try=$((_try + 1))
        log "rustup toolchain install failed; attempt $_try of 3"
    done
    run_vendor "rustup default $_toolchain" "$_bin/rustup" default "$_toolchain"
fi

# A non-root sandbox user can also use the shared registry cache, `cargo
# install` and install toolchains (rust-toolchain.toml).
chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME"

# Put the rustup proxies on the default PATH, for any PATH of the image.
# Symlinks keep the name that rustup dispatches on (argv[0]). Only free
# names and symlinks into $CARGO_HOME/bin get a new link. Other files and
# links stay as they are.
mkdir -p /usr/local/bin
for _f in "$_bin"/*; do
    [ -x "$_f" ] || continue
    _link="/usr/local/bin/${_f##*/}"
    if [ -L "$_link" ]; then
        case "$(readlink "$_link" 3>&-)" in
            "$_bin"/*) ln -sfn "$_f" "$_link" ;;
        esac
    elif [ ! -e "$_link" ]; then
        ln -s "$_f" "$_link"
    fi
done

# Login shells: /etc/profile resets PATH. Set the rustup homes and add
# $CARGO_HOME/bin back, for the commands that `cargo install` adds later.
mkdir -p /etc/profile.d
cat >/etc/profile.d/airlock-rust.sh <<'PROFILE'
# Managed by airlock: the shared Rust toolchain (rustup) and the
# commands from `cargo install`.
export RUSTUP_HOME="${RUSTUP_HOME:-/usr/local/rustup}"
export CARGO_HOME="${CARGO_HOME:-/usr/local/cargo}"
case ":$PATH:" in
    *":$CARGO_HOME/bin:"*) ;;
    *) PATH="$CARGO_HOME/bin:$PATH" ;;
esac
export PATH
PROFILE

"$_bin/rustup" --version </dev/null 3>&- || fail 12 "rustup --version failed"
if [ "$_toolchain" != none ]; then
    "$_bin/rustc" --version </dev/null 3>&- || fail 12 "rustc --version failed"
    "$_bin/cargo" --version </dev/null 3>&- || fail 12 "cargo --version failed"
fi
