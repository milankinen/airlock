# Cloud Hypervisor hardening (Linux part of the VM review)

The VM review of 2026-11-07 (see `2026-11-07-vm-review-fixes.md`) left a
list of open findings. This entry fixes the findings that only Linux and
the Cloud Hypervisor backend have.

## Landlock for virtiofsd and cloud-hypervisor

virtiofsd ran with `--sandbox none`. Its namespace sandbox needs user
namespaces, and many hosts do not give them to normal users (for example
Ubuntu with the AppArmor limit). A path bug in virtiofsd thus gave the
guest all files of the host user. cloud-hypervisor ran with no limit.

Fix: `vm/cloud_hypervisor/landlock.rs` makes a Landlock ruleset with raw
syscalls (no new crate). The ruleset is made in the parent, and a
`pre_exec` hook only calls `PR_SET_NO_NEW_PRIVS` and
`landlock_restrict_self`, which are async-signal-safe. Rules for each
virtiofsd:

- share dir: all rights (read rights for a read-only share)
- `vfs/` socket dir: all rights (socket and lock file)
- the virtiofsd executable and the system lib dirs, `/etc/ld.so.cache`,
  `/proc`: read and execute. (The review follow-up removed `/proc` for
  normal users, see `2026-11-08-cloud-hypervisor-review-fixes.md`.) The released virtiofsd links glibc
  dynamically, so the loader needs the lib dirs. `/proc` is for
  `/proc/self/fd` and `/proc/self/mountinfo`. Landlock blocks ptrace
  access to processes outside the domain, so `/proc/<pid>/environ` and
  `/proc/<pid>/root` of other processes stay closed.
- TCP bind and connect are handled with no rule (denied). With ABI 6+,
  abstract Unix sockets and signals are scoped to the domain.

ABI 1 is not used: it refuses every rename or link to another directory,
which breaks normal guest file operations. cloud-hypervisor gets its own
`--landlock` (rules from its config) only with ABI 3+, because its
Landlock code may need newer rights. Below that, airlock logs one warning
and the helpers run without limits.

virtiofsd also needed `--inode-file-handles=never`. The default `prefer`
opens the mount point of the share (for example `/home`) with `O_RDONLY`
to use `open_by_handle_at`, and Landlock refused that, so virtiofsd
exited at start. File handles need `CAP_DAC_READ_SEARCH`, so a normal
user never had them anyway.

The guest kernel configs now enable `CONFIG_SECURITY_LANDLOCK` (x86_64
also lists `landlock` in `CONFIG_LSM`). Sandboxes can use Landlock, and
nested airlock runs can test these rules.

Tested inside a sandbox with the new kernel (ABI 7): virtiofsd under the
rules serves a client, guest rename across dirs, hard links, deletes and
file mount sync work, as root and as a normal host user.

## serial.log had no size limit

cloud-hypervisor wrote the serial port to `serial.log`, and the guest
controls that output (`yes > /dev/ttyS0`). Now `--serial tty` sends it to
the stdout pipe of cloud-hypervisor, and stderr is a pipe too. A thread
per pipe copies up to 16 MiB into the log, writes a note, then reads and
drops the rest so that cloud-hypervisor never blocks.

## vsock handshake could lose bytes

The `CONNECT <port>` answer was read with a `BufReader`, which can read
past the `OK <port>\n` line and drop the first bytes of the guest data.
The handshake is now async, reads one byte at a time up to `\n` (at most
64 bytes) and has a 5 s timeout.

## Non-root image user saw project files as root

virtiofsd mapped only guest uid 0 (`--translate-uid map:0:<host>:1`), so a
non-root image user could not write the project, and git reported
"dubious ownership". Now virtiofsd has no translation, and the backend
gives the same guest idmaps as libkrun: host user to root on the layers
share, host user to the container user on the other shares. Guest init
already puts idmapped mounts on virtiofs shares, and virtiofsd accepts
them. A new test in `tests/vm/mounts.bats` checks this on all backends.

Image layer files still show as root on Cloud Hypervisor, and setuid bits
are lost. The image owners are in the `user.containers.override_stat`
xattr, which only the libkrun file server reads. A virtiofsd patch could
fix this. We chose not to carry a patch for now. The manual documents the
difference.

## File sync gave the host file the guest mode bits

After an atomic replace in the guest, the sync linked the new guest inode
to the host path. The host file then had the mode of the guest file (for
example 0777). The copy fallback forced 0600. Now the sync keeps the mode
of the source from the sandbox start, sets it on the guest file before
the link, and uses it for the copy.

This covers only a file that the guest replaced. A linked file is the
host file, so a guest chmod changes the host file at once (see the review
follow-up, `2026-11-08-cloud-hypervisor-review-fixes.md`).

## VM lifecycle

- The VM object now exists before the first virtiofsd starts. An error in
  the start drops it, which kills and reaps all processes that started.
  Before, the `Child` drop left them running.
- `vsock_connect` returns `VmStopped` when a process of the VM exited.
  A crash at start failed after 12 s of retries with a generic error. Now
  it fails at once with the tail of `cloud-hypervisor.log`.
- `stop` reports a process that exited with an error before the stop.
- The virtiofsd socket wait stops early when virtiofsd exits.

## Cleanup

- Removed the test and comment about guest symlinks at the log files. The
  sandbox dir is in the data dir now, and airlock makes the log files.
- Removed the cleanup of `vsock.sock_<port>`. cloud-hypervisor makes that
  file only when the host listens for guest connections, which airlock
  does not do.
