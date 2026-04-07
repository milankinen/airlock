# Fix VM networking by disabling TSI socket hijacking

Networking (DNS, TCP proxy) was broken on Linux because libkrun's
implicit vsock device enables TSI (Transparent Socket Impersonation)
with `TSI_HIJACK_INET`, which intercepts ALL AF_INET socket calls and
routes them through the host via vsock. This caused the supervisor's
DNS server (`10.0.0.1:53`) and TCP proxy (`127.0.0.1:15001`) to never
receive local connections — epoll notifications never fired because
the sockets were silently hijacked.

Fix: call `krun_disable_implicit_vsock()` then `krun_add_vsock(ctx, 0)`
to create an explicit vsock device with TSI disabled (flags=0). The
supervisor's port mapping via `krun_add_vsock_port2` still works for
the RPC channel. DNS resolution and TCP proxy redirect now functional.
