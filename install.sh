#!/bin/sh
set -eu

REPO="milankinen/ezpez"
INSTALL_DIR="${EZPEZ_INSTALL_DIR:-$HOME/.local/bin}"

info() { printf '  %s\n' "$@"; }
err()  { printf 'error: %s\n' "$@" >&2; exit 1; }

# ── Platform detection ──────────────────────────────────
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Darwin) OS="darwin" ;;
  Linux)  OS="linux" ;;
  *)      err "unsupported OS: $OS" ;;
esac

case "$ARCH" in
  arm64|aarch64) ARCH="aarch64" ;;
  x86_64)        ARCH="x86_64" ;;
  *)             err "unsupported architecture: $ARCH" ;;
esac

# Validate supported combinations
case "${OS}-${ARCH}" in
  darwin-aarch64|linux-x86_64|linux-aarch64) ;;
  *) err "unsupported platform: ${OS}-${ARCH}" ;;
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
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL=$(sha256sum "$TMPDIR/$ARCHIVE" | cut -d' ' -f1)
else
  ACTUAL=$(shasum -a 256 "$TMPDIR/$ARCHIVE" | cut -d' ' -f1)
fi
[ "$EXPECTED" = "$ACTUAL" ] || err "checksum mismatch: expected $EXPECTED, got $ACTUAL"

# ── Install ─────────────────────────────────────────────
info "extracting to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
tar -xzf "$TMPDIR/$ARCHIVE" -C "$TMPDIR"
mv "$TMPDIR/ez" "$INSTALL_DIR/ez"
chmod +x "$INSTALL_DIR/ez"

printf 'done! ez installed to %s/ez\n' "$INSTALL_DIR"
