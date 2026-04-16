# OCI image cache fast path

### What

Skip the network round-trip to resolve an OCI tag to a digest when the
sandbox already has a cached image matching the configured image name.
Also move the disk size display from `cmd_start.rs` (which computed
used/total via stat) to `vm.rs::log_config()` (which just shows the
configured size).

### Why

Every `airlock start` was doing a registry HEAD request to resolve the
image tag, even when the local cache was fully intact. This added 1-2s
of latency on every boot. The fast path checks whether the stored
`meta.json` name matches the config image name and the cache directory
has `rootfs` + `meta.json` — if so, it returns immediately without
touching the network.

The TTY gate at the top of `cmd_start.rs` was also moved down to only
guard the interactive preset prompt, allowing non-interactive runs to
proceed through config loading and validation (needed for bats tests).

### Design

- `oci::prepare()` reads `sandbox/image/meta.json` early and compares
  `meta.name` against the config image name
- On cache hit: log, create overlay dir, call `build_oci_image()` helper
- `build_oci_image()` extracted to avoid duplication between cache-hit
  and full-resolution paths
- `is_interactive()` check moved from top of `run()` to the dialoguer
  prompt — non-interactive mode now fails with a clear error instead of
  a blanket "TTY required"
