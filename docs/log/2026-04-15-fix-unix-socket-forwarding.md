# Fix Unix socket forwarding into the container

## Summary

Fixed Unix socket forwards (e.g. Docker socket) not being visible inside the
container. Three distinct bugs were found and fixed during diagnosis.

## Bugs

### 1. Stale inode from placeholder bind mount (original approach)

The original implementation created a placeholder regular file at
`/mnt/disk/sockets/{name}` and bind-mounted it into the container rootfs. A
bind mount captures the file's inode at mount time. `net::socket::start` then
called `remove_file` + `UnixListener::bind` at the same path, creating a new
inode. The bind mount in the container still pointed to the dead placeholder —
the container never saw the real socket.

### 2. Race: socket bound after container process starts

`net::socket::start` used `tokio::task::spawn_local` to create the listener.
The async tasks were queued but did not execute until the current async context
yielded. `process::spawn_user` (a synchronous fork) ran immediately after —
meaning the container process could start before any socket was bound.

**Fix:** `socket::start` (and similarly `dns::start`, `start_proxy`) now bind
their sockets synchronously before spawning the accept loop. The bind is not
async — `UnixListener::bind`, `UdpSocket::bind`, and `TcpListener::bind` are
all synchronous operations in tokio. All three functions now return
`anyhow::Result<()>` so a bind failure aborts init rather than being silently
swallowed.

### 3. Absolute symlink resolution outside chroot

The new approach bound the socket directly through the overlayfs mount point:
`UnixListener::bind("/mnt/overlay/rootfs/var/run/docker.sock")`. The container
image has `/var/run -> /run` (an absolute symlink). The supervisor process is
not chrooted — its root is `/`. When the kernel resolves the absolute symlink
`/run`, it follows it to the **VM's** `/run/`, not the container's `/run/`.
The socket was created at the wrong path.

**Fix:** `crate::util::resolve_in_root(root, guest_path)` walks the path
component by component; when it encounters an absolute symlink, it treats the
target as relative to `root` (the container root), mirroring chroot semantics.
The same function is now used for all path resolutions into the container
rootfs: socket binds, directory bind mount destinations, cache bind mount
destinations, and the `.airlock` mask destination.

## Architecture after fix

Socket forwards now work as follows:

1. `init::setup()` assembles the overlayfs rootfs at `/mnt/overlay/rootfs`.
2. `net::socket::start()` is called. For each socket:
   - Resolves the guest path with `resolve_in_root` (handles absolute symlinks).
   - Calls `UnixListener::bind` synchronously — socket file is created in the
     overlayfs upper layer via the VFS.
   - Spawns an async accept loop task.
3. All socket files exist before `process::spawn_user` is called.
4. The container sees the socket at its guest path through the overlayfs.
5. On connect, the accept loop relays the connection to the host-side socket
   via the `NetworkProxy` RPC.

## Files changed

- `crates/airlockd/src/util.rs` (new) — `resolve_in_root` utility
- `crates/airlockd/src/main.rs` — `mod util`; propagate errors from dns,
  proxy, and socket start
- `crates/airlockd/src/net/socket.rs` — synchronous bind before spawn;
  `resolve_in_root` for path; return `Result`
- `crates/airlockd/src/net/dns.rs` — bind socket before spawning serve task;
  return `Result`
- `crates/airlockd/src/net/proxy.rs` — bind listener before spawning accept
  loop; return `Result`
- `crates/airlockd/src/init/linux.rs` — use `resolve_in_root` for all
  guest-path-to-rootfs resolutions (dir mounts, cache mounts, `.airlock` mask)
