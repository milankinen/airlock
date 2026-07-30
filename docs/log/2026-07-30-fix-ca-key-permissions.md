# Protect the sandbox CA private key on disk

## Symptom

Each sandbox generates a local CA whose private key is stored in
`.airlock/sandbox/ca.json` and used to intercept the sandbox's TLS traffic.
The file was written world-readable (`std::fs::write`, mode 0644 under the
usual umask) inside a 0755 directory, so any other local user who could reach
the project directory could read the key and MITM the sandbox's intercepted
traffic. The write was also non-atomic: a crash mid-write left a truncated
`ca.json`, and because the next run only checks `ca.json.exists()` before
regenerating, it was never rebuilt — `read_ca` then failed to parse and the
sandbox couldn't start until the file was deleted by hand.

## Fix

- `generate_ca` now writes `ca.json` through `vault::atomic_write`, which
  creates the file mode 0600 and installs it with a tmp + rename, so the key
  is owner-only and a crash can't leave a half-written file in place.
- The `sandbox/` directory is additionally tightened to 0700 (best-effort) on
  each `lock()`, as defense in depth; the 0600 file mode is the real
  protection (a 0600 file is unreadable by other users regardless of directory
  mode).

## Tests

`project::tests::ca_key_is_written_owner_only` asserts the generated
`ca.json` has mode 0600.
