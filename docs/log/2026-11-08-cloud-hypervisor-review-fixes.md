# Cloud Hypervisor hardening: review fixes

A separate review of the hardening commit (see
`2026-11-08-cloud-hypervisor-hardening.md`) found no serious bugs, but
these smaller problems.

## File sync: mode claim was too strong

The comments and the commit said that the sync keeps the host file mode.
That is true only when the guest replaces the file (atomic save). A
writable file mount on the same filesystem is a hard link, so the guest
file is the host file: a guest `chmod` changes the host file at once,
and no sync event happens. There is also a short window between the
`fchmod` and the link. The comments now say this. A real fix would need a
copy in place of the link, and then direct writes would no longer reach
the host file at once.

## File sync: copy fallback lost bits to the umask

`PinnedDir::copy_from` made the file with `O_CREAT` and the mode, so the
umask applied (0664 became 0644). It now calls `fchmod` with the exact
mode after the open.

## Crash error could show an empty log tail

The cloud-hypervisor error output goes through a pipe and a copy thread
to its log. The crash error read the log as soon as the process exited,
maybe before the thread wrote the last lines. The backend now keeps the
thread handle and waits up to 1 s until the thread is finished before it
reads the tail. `stop` also waits after the reap. The wait in the vsock
connect is async, because the CLI runs a single-thread runtime and a
blocked thread would stop all other tasks.

## Boot wait had no total limit

Each vsock attempt of Cloud Hypervisor can take up to 5 s (handshake
timeout), and the boot made up to 60 attempts, so a hung guest could
block the start for 5 minutes. `VmInstance::vsock_connect` now uses one
deadline of 30 s for all attempts (`timeout_at`). 30 s, not 12 s: libkrun
attempts wait 200 ms for a refusal, so its old window was about 24 s.

## virtiofsd could read all of /proc

`/proc` gives the command lines of all host processes, and tokens in
arguments are common. virtiofsd needs from it:

- `/proc/self/fd/N` reopens: Landlock checks the rule of the target file,
  so this needs no `/proc` rule.
- `/proc/sys/fs/nr_open`: read at each start to raise the open file
  limit, also with `--rlimit-nofile`. It is now a file rule.
- `/proc/self/{uid_map,gid_map,setgroups}`: read only as root, to find
  out if it must drop its supplemental groups. If the read fails, it
  tries the drop, and that fails as root in a user namespace with
  `setgroups` denied. A rule cannot name `/proc/self` (it resolves to the
  pid of airlock), so `/proc` stays readable only when airlock runs as
  root.

Tested on a Landlock host: a normal user boots and uses the shares with
no `/proc` rule.

## Crash errors named the wrong process

`stop` gave the cloud-hypervisor error and log for a crashed virtiofsd,
and the socket wait named the share it waited for, not the share whose
virtiofsd stopped. The backend now tracks which process stopped and names
the share of a crashed virtiofsd. The error says to use the debug log
level for the virtiofsd log. The virtiofsd lines used a custom tracing
target that no log level let through, so they now use the default target.

## Second review

A second review found two more problems:

- The new message function had the same name as a macOS-only one in the
  same module, which breaks the macOS build. It has a new name.
- A comment in the boot code still said that the vsock connect retries
  for about 12 s.
