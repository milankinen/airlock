# Gate `net` module submodules on Linux target

The `net` module's implementation reaches into Linux-only APIs:
`libc::Ioctl`, `libc::mknod`, `/dev/net/tun`, `/sbin/ip` shell-outs,
raw `IFF_TUN` flags, etc. Previously this only mattered at link time
(airlockd only runs inside the guest VM), but on macOS even
`cargo check` for the workspace would fail because the submodules
compiled eagerly on the host target.

## Shape

Adopt the same pattern already used by `init.rs`:

- `net.rs` is the module's public face. It declares private
  submodules under `#[cfg(target_os = "linux")]` and re-exports the
  handful of entry points each sibling module needs
  (`start_dns`, `start_host_port_forward`, `start_host_socket_forward`,
  `start_tcp_proxy`, `open_local_tcp`, `DnsState`).
- `#[cfg(not(target_os = "linux"))]` stubs shadow those names with
  `unimplemented!("airlockd only runs inside the Linux VM")` so the
  rest of the crate type-checks on developer machines.
- Submodules are no longer `pub`. Nothing outside `net::*` reaches
  past the re-exports.

Callers renamed `net::dns::start` → `net::start_dns`,
`net::tcp_proxy::start` → `net::start_tcp_proxy`, etc. Shorter and
flatter.
