# Data directory length limit for the VM sockets

The VM bats tests failed in CI with `virtiofsd socket not ready after
5s: layers`. The managed sandbox moved to `<data>/boxes/<id>/`
(`2026-10-08-sandbox-data-dir.md`), so the VM sockets are now in the
data directory. In CI, the test HOME is in the repository checkout, and
the virtiofsd socket path had 110 bytes:

```
/home/runner/work/airlock/airlock/.tmp/tests/XXXXXXXX/home/.local/share/airlock/boxes/<id>/vfs/layers.sock
```

A Unix socket path on Linux has a limit of 107 bytes (108 with the NUL).
virtiofsd exits with `path must be shorter than SUN_LEN`. Airlock writes
the virtiofsd output only to the debug log, so the user saw only the
timeout. A user with a deep HOME or `XDG_DATA_HOME` gets the same error.

## Change

- `airlock start` checks the data directory first, before the KVM check.
  On Linux, a path longer than 70 bytes stops the start with exit code 1
  and tells the user to set a shorter `data_dir` in
  `~/.airlock/settings.toml`.
- The limit: `/boxes/<id>` adds 15 bytes, and the longest socket name
  adds 16 (`/vfs/layers.sock`, `/vsock.sock_1024`). The maximum is 76.
  70 keeps space for more mounts and new sockets.
- virtiofsd sockets use only the last part of the share tag:
  `files/rw` -> `vfs/rw.sock`, `files/ro` -> `vfs/ro.sock`. The guest
  still mounts by the full tag.
- macOS has no check. Its VM backend uses no Unix sockets, and the CLI
  socket already has a fallback to a short path.
- The bats helpers set `XDG_DATA_HOME` to a new `/tmp/airlock-test.*`
  directory (one for each CLI test, one for each VM test file) instead
  of removing it. The data directory path then has about 30 bytes.

## Alternatives

- Relative socket paths: start virtiofsd and cloud-hypervisor in the
  sandbox directory and connect from the host through
  `/proc/self/fd/<dirfd>/vsock.sock`. This removes the limit, but it is
  more complex. The data directory check is simple and gives a clear
  message.
- A check of each socket path in the Linux backend: exact, but the error
  comes only after the image pull.

## Not covered

Project-owned sandboxes (`<project>/.airlock/sandbox/`) do not depend on
the data directory. A deep project path can still exceed the limit.

## Tests

- `tests/cli/start.bats`: a deep `XDG_DATA_HOME` makes `airlock start`
  fail with the fix in the message. The test runs without KVM because
  the check comes before the KVM check.
