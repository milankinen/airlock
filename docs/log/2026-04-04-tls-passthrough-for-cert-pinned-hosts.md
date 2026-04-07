# TLS passthrough for cert-pinned hosts

Added `allowed_hosts_tls` config (supports globs like `*.example.com`)
for hosts whose TLS should not be intercepted. Supports tools with
certificate pinning that reject the MITM CA.

Flow: passthrough list sent to supervisor via RPC `tlsPassthrough`.
Supervisor proxy checks hostname against the list — if matched, skips
TLS MITM and forwards raw TLS bytes. Tells CLI `tls=false` so CLI
doesn't wrap with its own TLS. CLI also skips HTTP interception for
passthrough hosts (`http_engine = None`). Result: end-to-end TLS
between container and real server, no MITM, no HTTP parsing.
