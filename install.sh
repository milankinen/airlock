#!/bin/sh
set -eu

REPO="milankinen/ezpez"
INSTALL_DIR="${EZPEZ_INSTALL_DIR:-/usr/local/bin}"

info() { printf '  %s\n' "$@"; }
err()  { printf 'error: %s\n' "$@" >&2; exit 1; }

# ── Platform detection ──────────────────────────────────
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Darwin) OS="darwin" ;;
  *)      err "unsupported OS: $OS (only macOS is supported)" ;;
esac

case "$ARCH" in
  arm64|aarch64) ARCH="aarch64" ;;
  *)             err "unsupported architecture: $ARCH (only aarch64 is supported)" ;;
esac

# ── Version resolution ──────────────────────────────────
# Pinned at release time by CI; empty in the repo source.
DEFAULT_VERSION=""

if [ -n "${EZPEZ_VERSION:-$DEFAULT_VERSION}" ]; then
  VERSION="${EZPEZ_VERSION:-$DEFAULT_VERSION}"
else
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | grep '"tag_name"' | cut -d'"' -f4) \
    || err "failed to fetch latest version"
fi

ARCHIVE="ez-${VERSION}-${OS}-${ARCH}.tar.gz"
BASE_URL="https://github.com/$REPO/releases/download/$VERSION"

printf 'Installing ez %s (%s-%s)\n' "$VERSION" "$OS" "$ARCH"

# ── Download ────────────────────────────────────────────
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

info "downloading $ARCHIVE"
curl -fsSL "$BASE_URL/$ARCHIVE" -o "$TMPDIR/$ARCHIVE" \
  || err "download failed — does $VERSION exist?"
curl -fsSL "$BASE_URL/$ARCHIVE.sha256" -o "$TMPDIR/$ARCHIVE.sha256" \
  || err "checksum download failed"

# ── Verify checksum ─────────────────────────────────────
info "verifying checksum"
EXPECTED=$(cut -d' ' -f1 < "$TMPDIR/$ARCHIVE.sha256")
ACTUAL=$(shasum -a 256 "$TMPDIR/$ARCHIVE" | cut -d' ' -f1)
[ "$EXPECTED" = "$ACTUAL" ] || err "checksum mismatch: expected $EXPECTED, got $ACTUAL"

# ── Install ─────────────────────────────────────────────
info "extracting to $INSTALL_DIR"
tar -xzf "$TMPDIR/$ARCHIVE" -C "$TMPDIR"

if [ -w "$INSTALL_DIR" ]; then
  mv "$TMPDIR/ez" "$INSTALL_DIR/ez"
else
  sudo mv "$TMPDIR/ez" "$INSTALL_DIR/ez"
fi
chmod +x "$INSTALL_DIR/ez"

printf 'done! ez installed to %s/ez\n' "$INSTALL_DIR"
