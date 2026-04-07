# Add http: target prefix and distro presets

Extended target pattern syntax to `[http:]host[:port]`. The `http:` prefix
enforces HTTP-only traffic — non-HTTP connections are rejected after TLS
interception. Targets with `http:` also force TLS interception (no passthrough)
since the proxy needs to verify the protocol.

Rewrote copilot-cli preset based on the official GitHub allowlist reference
with path-restricting middleware for github.com, api.github.com, and
copilot-telemetry.githubusercontent.com. Added distro package manager presets
(alpine, debian, fedora, suse, arch) with http: enforcement.
