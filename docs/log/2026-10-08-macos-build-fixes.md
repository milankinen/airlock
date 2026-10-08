# Fix the macOS build, lint and tests

The last commits broke `mise airlock`, `mise lint` and `mise test` on
macOS. Linux was not affected, so the sandbox CI did not see it.

## Build tasks

- The kernel, initramfs and virtiofsd tasks give `CA_ARGS` to
  `docker run`. Outside the sandbox the array is empty. macOS has bash
  3.2, and bash before 4.4 treats `"${arr[@]}"` of an empty array as
  unbound under `set -u`. The tasks stopped with `CA_ARGS[@]: unbound
  variable`.
- The expansion is now `${CA_ARGS[@]+"${CA_ARGS[@]}"}`. It gives the
  quoted elements when the array is set, and nothing when it is empty.

## Lint

- `airlockd` `build_pre_exec`: only the Linux code reads `ns_fd`. The
  non-Linux build now discards it together with `harden`. A `cfg` on the
  `let` would make `sandbox_ns::fd()` dead code on macOS instead.
- `LayerTar::raw_file`: its only user, `test_layer_cache`, is Linux only.
  The helper now has the same `cfg`.

## Exec lookup test

- `exec_finds_nearest_running_sandbox_of_either_kind` wrote the sockets at
  `<sandbox>/cli.sock`. On macOS the temporary directory is deep
  (`/private/var/folders/...`), so the path of the project sandbox socket
  was 108 bytes. That is more than the 103 byte safe `sun_path` limit, so
  `cache::cli_sock_path` used the hashed fallback in `<data>/sock/`. The
  lookup did not see the file that the test wrote, and found the parent
  socket.
- The test now gets the socket paths from `cache::cli_sock_path`, the same
  as `airlock start`. It runs under `TempHome`, so the fallback sockets go
  into a temporary data directory and not into the user's data directory.
