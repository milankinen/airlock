#!/bin/sh
set -e

apk add --no-cache build-base bc flex bison perl linux-headers elfutils-dev openssl-dev xz findutils >/dev/null

KVER=6.18.13
wget -q "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KVER}.tar.xz"
tar xf "linux-${KVER}.tar.xz"
cd "linux-${KVER}"
cp /config .config
make olddefconfig
make -j"$(nproc)"

case "${ARCH}" in
  x86_64) cp arch/x86/boot/bzImage /out/Image ;;
  *)      cp arch/arm64/boot/Image /out/Image ;;
esac
