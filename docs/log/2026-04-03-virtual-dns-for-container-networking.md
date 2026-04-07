# Virtual DNS for container networking

### What

Virtual DNS server in the supervisor assigns synthetic IPs (10.2.0.0/16)
to hostnames. The proxy reverse-lookups these IPs to recover the
hostname, giving full hostname visibility for all connections including
plain HTTP.

### Virtual DNS design

Instead of forwarding real DNS queries, the supervisor runs a minimal
UDP DNS server on 10.0.0.1:53. When a DNS A query comes in (e.g., for
`example.com`), it allocates a virtual IP from the 10.2.0.0/16 block
and caches the bidirectional mapping. The container's `/etc/resolv.conf`
points to `nameserver 10.0.0.1`.

When the container connects to the virtual IP, iptables redirects to
the proxy. The proxy gets the original destination via SO_ORIGINAL_DST,
reverse-lookups the virtual IP → hostname, and forwards to the CLI
with the real hostname. The CLI resolves the hostname for real.

Uses `simple-dns` crate for DNS wire format and `scc::HashMap` for
lock-free concurrent lookups (shared between DNS server and proxy).

### TLS MITM trust

The project CA cert is installed into the container's trust store
during bundle preparation: read from the pristine image rootfs,
append the CA cert, write to the bundle rootfs. This makes curl,
wget, etc. trust the MITM certificates. Reading from the image
(not bundle) avoids duplicating the CA on repeated runs.

### VM clock

The VM has no RTC, so the system clock starts at epoch. The host's
current time is passed via kernel cmdline (`ezpez.epoch=<seconds>`)
and set in the init script with `date -s`.

### CLI refactoring

Separated network concerns: `cli/src/network/` module owns the TLS
config (`Arc<ClientConfig>` built once) and host_ports filtering.
Uses `rustls-native-certs` for the macOS system certificate store
instead of bundled `webpki-roots`.
