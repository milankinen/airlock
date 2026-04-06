#!/bin/bash
set -euo pipefail

mkdir -p sandbox/out
rm -f sandbox/out/libkrun*

# Download libkrunfw from GitHub releases
KRUNFW_VERSION="v5.3.0"
HOST_ARCH=$(uname -m)
echo "Downloading libkrunfw ${KRUNFW_VERSION} (${HOST_ARCH})..."
KRUNFW_FULL_VERSION="${KRUNFW_VERSION#v}"
curl -fSL "https://github.com/containers/libkrunfw/releases/download/${KRUNFW_VERSION}/libkrunfw-${HOST_ARCH}.tgz" \
  | tar -xzf - --strip-components=1 -C sandbox/out "lib64/libkrunfw.so.${KRUNFW_FULL_VERSION}"
mv "sandbox/out/libkrunfw.so.${KRUNFW_FULL_VERSION}" sandbox/out/libkrunfw.so
echo "libkrunfw: sandbox/out/libkrunfw.so ($(du -h sandbox/out/libkrunfw.so | cut -f1))"

# Build libkrun
echo "Building libkrun..."
docker run --rm \
  -v "$PWD/sandbox/out:/out" \
  -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)" \
  rust:1-slim-trixie sh -c '
    set -e
    apt-get update -qq && apt-get install -y -qq git make libclang-dev libcap-ng-dev >/dev/null 2>&1
    cd /tmp
    git clone --depth=1 --branch v1.17.4 https://github.com/containers/libkrun.git
    cd libkrun
    make BLK=1
    cp target/release/libkrun.so.* /out/libkrun.so
    chown "$HOST_UID:$HOST_GID" /out/libkrun.so
  '
echo "libkrun: sandbox/out/libkrun.so ($(du -h sandbox/out/libkrun.so | cut -f1))"

# Touch outputs so mise detects them as fresh
touch sandbox/out/libkrun.so sandbox/out/libkrunfw.so
