#!/bin/sh
set -e

apk add --no-cache busybox-extras iproute2 cpio crun iptables e2fsprogs >/dev/null 2>&1

mkdir -p /proc /sys /dev /dev/pts /tmp /run /usr/bin
cp /init-script /init && chmod 755 /init
cp /supervisor-bin /usr/bin/supervisor && chmod 755 /usr/bin/supervisor

cd /
find . \( -path './proc/*' -o -path './sys/*' -o -path './dev/*' -o -path './tmp/*' -o -path './out/*' \) -prune -o -print \
  | cpio -o -H newc --quiet | gzip > /out/initramfs.gz
