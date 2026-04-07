# Fix CA cert mounting via overlayfs lowerdir

## Problem

Custom CA certificates (for MITM TLS interception) were not visible inside
the container. `curl` reported:

```
curl: (77) error setting certificate file: /etc/ssl/certs/ca-certificates.crt
cat: /etc/ssl/certs/ca-certificates.crt: No such file or directory
```

## Root cause of the original approach

`install_ca_cert` was writing the combined cert bundle to `overlay/files_rw/`
and adding entries to `mounts.json` so the supervisor would create symlinks
inside the rootfs. However, the `mounts.json` file-mount symlink path was
broken — the files weren't being found by curl because the symlinks either
didn't exist or pointed nowhere accessible.

## Fix: use an extra overlayfs lowerdir

Instead of symlinks, the CA cert bundle is now written directly into
`overlay/ca/<distro-path>` (e.g. `overlay/ca/etc/ssl/certs/ca-certificates.crt`).

The supervisor's `assemble_rootfs` checks whether `/mnt/overlay/ca` exists
and, if so, prepends it as the highest-priority lowerdir:

```
lowerdir=/mnt/overlay/ca:/mnt/base
```

This means the CA cert file appears as a regular file inside the container
(not a symlink), which is compatible with curl's `CURLOPT_CAINFO` loading.

## Why the upper dir whiteout caused a red herring

After switching to the lowerdir approach, the cert was still missing. The
cause was a whiteout entry in the overlayfs upper dir
(`/mnt/disk/overlay/rootfs/etc/ssl/certs/.wh.ca-certificates.crt`) left
from a previous corrupted run. The whiteout shadowed the lowerdir file.
Clearing the disk state fixed it. The `reset_overlay_if_needed` logic handles
this for image changes; corrupted state requires a manual disk wipe or a new
image ID.
