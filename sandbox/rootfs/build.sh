#!/bin/sh
set -e

apk add --no-cache busybox-extras iproute2 cpio crun iptables e2fsprogs e2fsprogs-extra tar >/dev/null 2>&1

mkdir -p /proc /sys /dev /dev/pts /tmp /run /usr/bin /ez
cp /init-script /init && chmod 755 /init
cp /supervisor-bin /usr/bin/supervisor && chmod 755 /usr/bin/supervisor
cp /ez-oci-run /ez/oci-run && chmod 755 /ez/oci-run
cp /ez-oci-exec /ez/oci-exec && chmod 755 /ez/oci-exec

cd /

# Gzipped cpio archive (macOS / Apple Virtualization)
find . \( -path './proc/*' -o -path './sys/*' -o -path './dev/*' -o -path './tmp/*' -o -path './out/*' \) -prune -o -print \
  | cpio -o -H newc --quiet | gzip > /out/initramfs.gz

# Gzipped tar archive (Linux - extracted at runtime)
tar -czf /out/rootfs.tar.gz \
  --exclude='./proc/*' --exclude='./sys/*' --exclude='./dev/*' --exclude='./tmp/*' --exclude='./out/*' \
  .
