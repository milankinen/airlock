#!/bin/bash
# Install the sandbox build tooling: the mold linker and the kache rustc
# cache. Runs inside the sandbox with an open network:
#
#   mise sandbox-setup
#
# Safe to run again (e.g. after a KACHE_VERSION bump).
set -euo pipefail

KACHE_VERSION="1.0.0"
CACHE_DIR=/cache

SUDO=
[[ "$(id -u)" == 0 ]] || SUDO=sudo

case "$(uname -m)" in
  x86_64)        ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

$SUDO mkdir -p "$CACHE_DIR/kache" "$CACHE_DIR/project"
$SUDO chown "$(id -u):$(id -g)" "$CACHE_DIR/kache" "$CACHE_DIR/project"

# mold, plus the system packages of the project build (capnp schemas,
# the musl build of airlockd)
PACKAGES=(mold capnproto libcapnp-dev musl-tools)
if dpkg -s "${PACKAGES[@]}" >/dev/null 2>&1; then
  echo "System packages already installed: ${PACKAGES[*]}"
else
  echo "Installing ${PACKAGES[*]}..."
  $SUDO apt-get update -qq
  $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${PACKAGES[@]}" >/dev/null
fi

# The project toolchain (rust-toolchain.toml) with clippy and its musl
# target, plus nightly rustfmt for `mise format` / `mise lint`
echo "Installing the Rust toolchains..."
(
  cd "$(dirname "$0")/../.."
  rustup toolchain install
  rustup component add clippy
  rustup target add "${ARCH}-unknown-linux-musl"
)
rustup toolchain install nightly --profile minimal --component rustfmt

# kache
if [[ "$(kache --version 2>/dev/null)" == *"$KACHE_VERSION"* ]]; then
  echo "kache already installed: $(kache --version)"
else
  echo "Installing kache ${KACHE_VERSION}..."
  TARBALL="kache-${ARCH}-unknown-linux-musl.tar.gz"
  URL="https://github.com/kunobi-ninja/kache/releases/download/v${KACHE_VERSION}/${TARBALL}"
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  curl -fsSL -o "$TMP/$TARBALL" "$URL"
  curl -fsSL -o "$TMP/$TARBALL.sha256" "$URL.sha256"
  (cd "$TMP" && sha256sum -c "$TARBALL.sha256")
  tar -xzf "$TMP/$TARBALL" -C "$TMP" kache
  $SUDO install -m 0755 "$TMP/kache" /usr/local/bin/kache
fi

# Cargo: link with mold, compile through kache. Only the host (gnu)
# targets use mold; the musl target of airlockd keeps its default linker.
CARGO_CONFIG="${CARGO_HOME:-$HOME/.cargo}/config.toml"
echo "Writing $CARGO_CONFIG"
mkdir -p "$(dirname "$CARGO_CONFIG")"
cat >"$CARGO_CONFIG" <<'EOF'
# Managed by .claude/sandbox/setup.sh
[build]
rustc-wrapper = "/usr/local/bin/kache"

[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=mold"]

[target.aarch64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
EOF

# kache: store in /cache, fast compression, half of the disk as budget.
DISK_BYTES="$(df -B1 --output=size "$CACHE_DIR" | tail -n 1 | tr -d ' ')"
MAX_SIZE="$((DISK_BYTES / 2 / 1024 / 1024))MiB"
KACHE_CONFIG_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/kache/config.toml"
echo "Writing $KACHE_CONFIG_FILE (max size ${MAX_SIZE})"
mkdir -p "$(dirname "$KACHE_CONFIG_FILE")"
cat >"$KACHE_CONFIG_FILE" <<EOF
# Managed by .claude/sandbox/setup.sh
[cache]
local_store = "$CACHE_DIR/kache"
compression_level = 1
local_max_size = "$MAX_SIZE"
EOF

kache doctor || true
echo "Sandbox setup done"
