#!/bin/sh
set -e

apk add --no-cache build-base bc flex bison perl linux-headers elfutils-dev openssl-dev xz findutils >/dev/null

# The working directory can be a persistent build tree (see
# mise/tasks/build/kernel): reuse the download, sources and objects.
KVER=6.18.13
if [ ! -f "linux-${KVER}.tar.xz" ]; then
  wget -q -O "linux-${KVER}.tar.xz.part" "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KVER}.tar.xz"
  mv "linux-${KVER}.tar.xz.part" "linux-${KVER}.tar.xz"
fi
if [ ! -f "linux-${KVER}/.extracted" ]; then
  rm -rf "linux-${KVER}"
  tar xf "linux-${KVER}.tar.xz"
  touch "linux-${KVER}/.extracted"
fi
cd "linux-${KVER}"
cp /config .config
make olddefconfig
make -j"$(nproc)"

case "${ARCH}" in
  x86_64) cp arch/x86/boot/bzImage /out/Image ;;
  *)      cp arch/arm64/boot/Image /out/Image ;;
esac
