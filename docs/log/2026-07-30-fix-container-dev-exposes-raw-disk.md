# Fix container exposure of the raw project disk via /dev

## Symptom

A process inside the sandbox container, running as root, could read and write
`/dev/vda` — the raw ext4 project disk — directly. That bypasses the overlay
view and the file-mask bind mounts (which only hide paths at the filesystem
layer), giving the container access to the overlay upperdir, other caches, and
masked userdata that the masking is supposed to keep out of reach.

## Root cause

Container `/dev` was set up with a recursive bind of the VM's entire `/dev`
(`bind_rec("/dev", …)`), which pulls in every device node the VM has —
including block devices (`/dev/vda`, loop devices, etc.). The comment
justified it as "avoids mknod; all devices already present", trading safety
for convenience.

## Fix

Replaced the recursive bind with a fresh `tmpfs` at the container's `/dev`,
populated with only the OCI runtime default device set:

- char nodes: `null`, `zero`, `full`, `random`, `urandom`, `tty` (always),
  plus `fuse` when present (BuildKit / rootless overlay tooling) and `kvm`
  when nested virtualization is enabled;
- the standard symlinks: `fd`, `stdin`, `stdout`, `stderr` → `/proc/self/fd*`,
  and `ptmx` → `pts/ptmx`.

Each node is bind-mounted from the VM's `/dev` (a new `bind_dev_node` helper),
so we still avoid `mknod` and don't hardcode major/minor — but block/VM
devices are simply never named, so they can't reach the container. `MS_NODEV`
is intentionally omitted on the `/dev` tmpfs so the bound char devices work;
`MS_NOSUID` is set.

`mount::bind_rec` had no other caller and was removed.

## Compatibility note

A container that expected a device outside this set (e.g. a raw block device,
`/dev/loop*`, `/dev/net/tun`) will no longer find it. That exposure was the
vulnerability; workloads needing a persistent non-overlay filesystem already
use `/airlock/disk`. If a future workload legitimately needs another *char*
device, add it to the allowlist in `container::setup`.

## Testing

Verified `cargo clippy -p airlockd --tests` and `mise lint` are clean. The
`/dev` layout is exercised by the VM bats suite (`bats:vm`), which requires
KVM + Docker and was not run in this environment.
