#!/bin/bash
set -euo pipefail

mkdir -p sandbox/out
rm -f sandbox/out/libkrun* sandbox/out/libkrunfw*

KRUNFW_VERSION="v5.3.0"
KRUN_VERSION="v1.17.4"

# Build libkrunfw with netfilter support
echo "Building libkrunfw (with netfilter)..."
docker run --rm \
  -v "$PWD/sandbox/libkrun/netfilter.cfg:/netfilter.cfg:ro" \
  -v "$PWD/sandbox/out:/out" \
  -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)" \
  rust:1-slim-trixie sh -c "
    set -e
    apt-get update -qq && apt-get install -y -qq \
      git make gcc curl xz-utils python3 python3-pyelftools flex bison bc \
      libelf-dev libssl-dev >/dev/null 2>&1
    cd /tmp
    git clone --depth=1 --branch ${KRUNFW_VERSION} https://github.com/containers/libkrunfw.git
    cd libkrunfw
    # Enable netfilter in kernel config
    sed -i '/# CONFIG_NETFILTER is not set/d' config-libkrunfw_\$(uname -m)
    cat /netfilter.cfg >> config-libkrunfw_\$(uname -m)
    make -j\$(nproc)
    cp libkrunfw.so.* /out/libkrunfw.so
    chown \"\$HOST_UID:\$HOST_GID\" /out/libkrunfw.so
  "
echo "libkrunfw: sandbox/out/libkrunfw.so ($(du -h sandbox/out/libkrunfw.so | cut -f1))"

# Build libkrun with block device support
echo "Building libkrun..."
docker run --rm \
  -v "$PWD/sandbox/out:/out" \
  -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)" \
  -e "KRUN_VERSION=${KRUN_VERSION}" \
  rust:1-slim-trixie sh -c '
    set -e
    apt-get update -qq && apt-get install -y -qq git make libclang-dev libcap-ng-dev >/dev/null 2>&1
    cd /tmp
    git clone --depth=1 --branch "$KRUN_VERSION" https://github.com/containers/libkrun.git
    cd libkrun
    make BLK=1
    cp target/release/libkrun.so.* /out/libkrun.so
    chown "$HOST_UID:$HOST_GID" /out/libkrun.so
  '
echo "libkrun: sandbox/out/libkrun.so ($(du -h sandbox/out/libkrun.so | cut -f1))"

# Touch outputs so mise detects them as fresh
touch sandbox/out/libkrun.so sandbox/out/libkrunfw.so
