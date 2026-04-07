# Fix socket forwarding: tilde expansion and guest-path routing

Two related bugs in the Unix socket forwarding feature:

## Guest path sent to CLI instead of host path

The supervisor was sending the raw host socket path in `NetworkProxy.connect`,
which meant the CLI tried to connect to e.g. `~/.ssh/agent.sock` literally
(tilde not expanded). The fix: supervisor sends the **guest path** instead, and
the CLI maps guest → host using `Network.socket_map`. This also means the
supervisor never needs to know the host path at all.

## Tilde expansion for both host and guest paths

- **Host `~`**: expanded in `network::setup()` using `dirs::home_dir()`
- **Guest `~`**: expanded using `container_home` from the OCI bundle
  (e.g. `~/docker.sock` → `/root/docker.sock` for a root container)

`container_home` is already computed by `oci::prepare()` when reading
`/etc/passwd` from the image rootfs. It is now stored on `Bundle` and passed
to `network::setup()`.

## Single source of truth: `Network.socket_map`

`Network.socket_map` (`expanded_guest → expanded_host`) is built once with
all tildes resolved. `supervisor::start()` now reads socket forwards from
`network.socket_map` before moving `network` into the RPC capability, instead
of re-reading and re-expanding from the raw project config. This eliminates
the duplicate expansion logic.
