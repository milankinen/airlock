# WIP — Transparent network proxy via vsock

### What

Infrastructure for transparent TCP proxy: iptables redirect in the VM,
TLS MITM with per-project CA, push-based TcpSink RPC for bidirectional
relay, CLI-side NetworkProxy that makes real outbound connections.

### Status

- Kernel: netfilter/iptables/NAT modules enabled ✓
- Init: iptables REDIRECT + dummy default route ✓
- Supervisor: proxy listener on port 8080 ✓
- Supervisor: TLS interceptor (rcgen + rustls) ✓
- RPC: NetworkProxy + TcpSink push-based schema ✓
- CLI: NetworkProxy impl with real TCP + TLS ✓
- CA: auto-generated per project, mounted into VM ✓
- **BUG**: Relay chain hangs — proxy accepts connections but
  data doesn't flow through the RPC relay to the host. Needs
  debugging of the TcpSink bidirectional relay.
