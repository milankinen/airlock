#!/bin/bash
# Build the libkrun dynamic library with the C API (macOS arm64).
# Usage: build.sh <src-dir> <output-file>
#   <src-dir>:     libkrun checkout. The script clones it if it is missing.
#   <output-file>: Path of the built libkrun.dylib.
set -euo pipefail

# libkrun 2.0 (unreleased) is pinned to a commit: the free page reporting
# mode that gives guest memory back to macOS exists only on main.
LIBKRUN_REPO="https://github.com/containers/libkrun.git"
LIBKRUN_COMMIT="a0375321bb1e6e51312e022af53e9e7668d3a07e"

SRC="$1"
OUTPUT="$2"

if [[ ! -d "$SRC/.git" ]]; then
  git clone --quiet "$LIBKRUN_REPO" "$SRC"
fi
if [[ "$(git -C "$SRC" rev-parse HEAD)" != "$(git -C "$SRC" rev-parse "${LIBKRUN_COMMIT}^{commit}" 2>/dev/null)" ]]; then
  git -C "$SRC" fetch --quiet origin
  git -C "$SRC" checkout --quiet --detach "$LIBKRUN_COMMIT"
fi

# Build only the VMM library with the C API. `make` also builds the guest
# init library of libkrun, which airlock does not use.
# No --target: the caller makes sure that the host is arm64 macOS, as for
# airlock. The default deployment target of rustc is the same as for airlock.
(
  cd "$SRC"
  CARGO_TARGET_DIR="$PWD/target" cargo build --release -p libkrun --features ffi,blk
)
cp "$SRC/target/release/libkrun.dylib" "$OUTPUT"
