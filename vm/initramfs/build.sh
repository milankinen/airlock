#!/bin/sh
set -e

# Install runtime packages.
# cpio is a build tool only — it goes into / after the snapshot, not the initramfs.
apk add --no-cache busybox-extras iproute2 iptables e2fsprogs e2fsprogs-extra >/dev/null 2>&1

# Set up the container filesystem
mkdir -p /proc /sys /dev /dev/pts /tmp /run /usr/bin
cp /init-script /init && chmod 755 /init
cp /supervisor-bin /usr/bin/ezd && chmod 755 /usr/bin/ezd

# Snapshot the rootfs into /staging before installing build tools.
# Exclude volatile/host-only dirs so they don't end up in the archives.
mkdir /staging
cd /
tar -c \
  --exclude=./proc --exclude=./sys --exclude=./dev --exclude=./tmp \
  --exclude=./out --exclude=./staging \
  . | tar -xC /staging
# Recreate the empty mount-point placeholders that were excluded above.
mkdir -p /staging/proc /staging/sys /staging/dev /staging/dev/pts /staging/tmp

# Install cpio (lands in /, not /staging — build tool only)
apk add --no-cache cpio >/dev/null 2>&1

cd /staging

# Gzipped cpio archive for Apple Virtualization / cloud-hypervisor
find . | cpio -o -H newc --quiet | gzip > /out/initramfs.gz
