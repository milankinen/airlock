# Host I/O security

The sandbox writes the project dir, `.airlock/` and rw mounts. It can
plant symlinks, FIFOs and hard links there, and swap dirs. Airlock
followed such links at the next start. This entry replaces six earlier
entries. The manual page "I/O security" has the design.

## Design

* All host paths are `VerifiedPath` values. `new` and `join` refuse a
  symlink in each component below a root and refuse `..`. `new_follow`
  and `join_follow` exist for user-picked entry points, with an optional
  scope.
* Each file operation is a method of `VerifiedPath` and checks again.
  The last name must not be a symlink or FIFO. Entry operations
  (`remove_file`, `remove_dir_all`, `rename_to`, `symlink_metadata`,
  `read_link`) accept a link.
* `Context` has `home`, `work` and `data: DataDir`. `Context::new` fails
  at once when `HOME` is unset or the cwd is gone. Functions take
  `&VerifiedPath` or `&DataDir`.
* Mount sources, `vm.kernel` and `vm.initramfs` that enter a guest-writable
  dir must stay in it (`check_stays_in`, link-by-link walk, 40 links max).
  The check runs before the `missing` action.
* Host sockets in guest-writable dirs are pinned at start (inode, link
  count 1). Other sockets are not pinned.
* The config-change check compares resolved host paths with the last run.
* `PinnedDir` stays for the file sync only.
* Clippy bans `Path`, `PathBuf`, `std::fs`, `tokio::fs`, path probes,
  Unix socket bind and connect, `symlink`, `chown`. Only `util/verified_path.rs`,
  `util/safe_fs.rs`, `build.rs` and the test allow in `main.rs` may
  allow them. No tool checks the allows.

## Decisions

* A path type that keeps the given and the resolved path, and checks the
  path again before each operation, instead of fd-based I/O everywhere.
* The path type has the file operations as methods. A lint bans plain
  path types and the std file calls outside a short list of files.
* The context has the home, work and data dirs as checked paths. It
  stops at start when HOME is not set or the current dir is gone.
* Missing dirs are allowed at any level. Only symlinks are refused.
* The check-to-use window while a VM runs is accepted. File sync stays
  fd-based, because it runs while the guest writes.
* Data-dir sandboxes are the recommended kind. Project sandboxes are
  known to be less safe.
* Host paths in the config are resolved before the config-change
  check, and the resolved paths are compared.
* Links are followed only at entry points that the user picks.
* One path error for all refused symlinks. `airlock rm` still removes a
  symlinked `.airlock` link.
* Lead choice: a host socket is pinned only in a guest-writable dir, so
  that a host service that restarts keeps working.

## Rejected

* A fd-based `util::fs` with many scope modes. Too heavy for the gain.
* A ban of only `std::fs`, without the path type ban. A plain path could
  still travel between modules and reach a file call unchecked.

## Behavior changes

* Refused symlinks in the sandbox dir, `.airlock/`, image cache and data
  dir give a path error.
* A mount source that leaves a guest-writable dir stops the start with a
  hint.
* A relative host socket path resolves from the project dir.
* The start asks when a resolved host path changed.
* Airlock stops at once when `HOME` is unset or the cwd is gone.
* The vault gives an error and does not use a file in the cwd when it
  cannot find the vault file.
* `airlock rm` removes a symlinked `.airlock` and never follows it.

## Residual risks

* Check-to-use race while a VM runs. Each operation checks again, but the
  check is not atomic with the use. Accepted.
* A link that changes after the start does not change running mounts. The next
  start checks again.
* Data sub-dirs made after a check are not pinned unless `reverify` runs.

## Tests

Rust tests and lint ran on Linux. The macOS build and the bats/VM tests
did not run here.
