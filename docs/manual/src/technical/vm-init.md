# VM init

Inside the VM, the kernel hands off to a small init in the initramfs
that mounts essential filesystems (`/proc`, `/sys`, `/dev`) and then
launches the supervisor (`airlockd`). The supervisor's `setup` closure
(driven by the first `start` RPC) does the heavy lifting.

Each stage is a small submodule under `airlockd/src/init/linux/`;
`init/linux.rs` is just the ordering glue. The order is
load-bearing — VirtioFS shares must be mounted before the overlay
can reference them, networking must be up before the proxy begins
accepting connections, and container mounts run last so file bind
mounts win over earlier dir bind mounts.

## Stages

1. **Clock** (`clock::set`) — the host passes Unix epoch + nanos in
   the `start` RPC; the guest sets the system clock so timestamps are
   correct from the start.

2. **VirtioFS shares** (`mount::virtiofs`):
   - `layers` — shared per-layer OCI cache (read-only).
   - One share per configured directory mount (tag `project` for the
     project dir; `dir_0`, `dir_1`, … for others).
   - `files/rw` and/or `files/ro` — only mounted when the config has
     any file mounts of that kind.

3. **Networking** (`net::setup`) — loopback with `10.0.0.1/32` for
   the in-VM DNS server and sysctl tuning. The default route is
   installed later by `tcp_proxy::start` once `airlock0` (the TUN
   device) is up. No iptables rules are required.

4. **Project disk** (`disk::setup`) — formats the ext4 image if blank
   (`mkfs.ext4`), mounts it at `/mnt/disk`. Resizes the filesystem if
   the disk image was enlarged since the last boot.

5. **Overlay assembly** (`overlay::assemble`):
   - **CA tmpfs** (`ca::prepare_overlay`): if the `caCert` field is
     non-empty, a small tmpfs is created at `/mnt/ca-overlay` with a
     per-distro CA bundle — the image's own bundle (read from the
     topmost layer that ships it) with the project CA appended. The
     tmpfs is then spliced on top of the image lowerdir stack, so the
     CA is present without writes landing on the persistent upperdir.
     The raw CA is also dropped at distro anchor paths
     (`/usr/local/share/ca-certificates/airlock.crt`, …) so
     trust-update tools regenerate bundles that still include it.
   - **Lowerdir stack**: `/mnt/ca-overlay` (when present) →
     `/mnt/layers/<digest>/` per image layer, topmost-first.
     Mounted with `userxattr` so overlayfs honors whiteouts encoded
     by the host-side extractor as `user.overlay.whiteout` /
     `user.overlay.opaque` xattrs (requires kernel ≥ 5.11).
   - **Upper + work**: on the ext4 disk at `/mnt/disk/overlay/rootfs`
     and `/mnt/disk/overlay/work`. The overlay upper is reset if the
     stored image ID (`.image_id` on disk) differs from the one
     passed in `start`.

6. **DNS** (`net::setup_dns`) — writes `nameserver 10.0.0.1` into the
   composed rootfs's `/etc/resolv.conf`. Queries go to the in-VM
   network proxy, which resolves them on the host.

7. **Container mounts** (`container::setup`) — mounts `proc`, `sys`,
   `dev`, cgroup2, and applies file bind mounts and cache bind mounts
   inside the rootfs. Runs after overlay assembly so file bind mounts
   can override paths inside dir-bind-mounted directories.

The overlay upper layer on disk means writable container state
persists across runs. When the base image changes, the stored image
ID triggers a full upper reset.
