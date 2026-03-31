#!/bin/sh
set -e

OUTDIR="$1"
MINIROOTFS_URL="https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/aarch64/alpine-minirootfs-3.23.3-aarch64.tar.gz"

mkdir -p "$OUTDIR/rootfs"

# Download minirootfs if not cached
if [ ! -f "$OUTDIR/rootfs/bin/busybox" ]; then
    echo "  downloading alpine minirootfs..."
    curl -sL "$MINIROOTFS_URL" | tar xz -C "$OUTDIR/rootfs"
fi

# Copy config.json
cp "$(dirname "$0")/config.json" "$OUTDIR/config.json"

echo "  bundle ready at $OUTDIR"
