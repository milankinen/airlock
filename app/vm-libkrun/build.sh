#!/bin/bash
# Build the libkrun dynamic library with the C API (macOS arm64).
# Usage: build.sh <src-dir> <output-file>
#   <src-dir>:     libkrun checkout. The script clones it if it is missing.
#   <output-file>: Path of the built libkrun.dylib.
set -euo pipefail

# Pinned unreleased 2.0 commit: free page reporting exists only on main.
LIBKRUN_REPO="https://github.com/containers/libkrun.git"
LIBKRUN_COMMIT="1156ba7f7aa016b8b16b15e7fd278f65a57716a0"

SRC="$1"
OUTPUT="$2"

if [[ ! -d "$SRC/.git" ]]; then
  git clone --quiet "$LIBKRUN_REPO" "$SRC"
fi
if [[ "$(git -C "$SRC" rev-parse HEAD)" != "$(git -C "$SRC" rev-parse "${LIBKRUN_COMMIT}^{commit}" 2>/dev/null)" ]]; then
  git -C "$SRC" fetch --quiet origin
  git -C "$SRC" checkout --quiet --detach "$LIBKRUN_COMMIT"
fi

# Build only the VMM library. `make` also builds the unused guest init.
# No --target: the caller makes sure that the host is arm64 macOS.
(
  cd "$SRC"
  CARGO_TARGET_DIR="$PWD/target" cargo build --release -p libkrun --features ffi,blk
)
cp "$SRC/target/release/libkrun.dylib" "$OUTPUT"
