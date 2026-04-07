# Plan: Replace libkrun with Cloud Hypervisor + virtiofsd

## Context

libkrun has several issues on Linux:
- VirtioFS doesn't support xattrs needed for kernel overlayfs copy-up
- TSI socket hijacking breaks local networking (requires explicit disable)
- Implicit console steals stdin/stdout
- Requires building both libkrun.so and libkrunfw.so from source
- Embedding two shared libraries and dlopen'ing them is fragile

Cloud Hypervisor (CH) + external virtiofsd is the standard production
approach (used by Kata Containers). Both are static binaries — no dynamic
linking, no dlopen. virtiofsd supports `--xattr` for trusted xattr
passthrough, enabling kernel overlayfs without fuse-overlayfs.

## Architecture

```
Host:
  ez (CLI binary)
   ├── virtiofsd processes (one per VirtioFS share)
   │    └── socket: .ezpez/projects/<p>/vfs-<tag>.sock
   ├── cloud-hypervisor subprocess
   │    ├── --kernel Image (vmlinux)
   │    ├── --initramfs initramfs.gz
   │    ├── --fs tag=base,socket=vfs-base.sock ...
   │    ├── --disk path=disk.img
   │    ├── --vsock cid=3,socket=vsock.sock
   │    └── --api-socket api.sock
   └── connect to vsock.sock_1024 (CONNECT 1024\n)

Guest (our kernel + initramfs):
  /init → supervisor
   ├── mount -t virtiofs base /mnt/base
   ├── mount -t overlay ... /mnt/overlay/rootfs
   ├── bind mounts, networking, cache disk
   └── crun --bundle /mnt/overlay
```

## Detailed plan

### Phase 1: Build tasks

**`mise/tasks/build/cloud-hypervisor`** — new task:
- Download CH static binary from GitHub releases (v51.1)
- x86_64: `cloud-hypervisor-static`, aarch64: `cloud-hypervisor-static-aarch64`
- Output: `sandbox/out/cloud-hypervisor`

**`mise/tasks/build/virtiofsd`** — new task:
- Download virtiofsd static binary from GitHub releases (v1.16)
- x86_64: `virtiofsd-x86_64`, aarch64: `virtiofsd-aarch64`
- Output: `sandbox/out/virtiofsd`

**`mise/tasks/build/kernel`** — enable for Linux:
- Remove OS gate (currently macOS-only)
- x86_64 config already has all needed features
- Output: vmlinux as `sandbox/out/Image`

**`mise/tasks/build/dev`** — update deps:
- Remove `build:libkrun` dependency
- Add `build:cloud-hypervisor`, `build:virtiofsd`, `build:kernel` for Linux

### Phase 2: Assets

**`cli/build.rs`** — Linux checksum includes:
- `sandbox/out/Image` (kernel)
- `sandbox/out/initramfs.gz`
- `sandbox/out/cloud-hypervisor`
- `sandbox/out/virtiofsd`

**`cli/src/assets.rs`** — Linux assets:
- Extract kernel Image, initramfs.gz, cloud-hypervisor, virtiofsd
- All to `~/.ezpez/kernel/`
- chmod +x on binaries

### Phase 3: VM backend — `cli/src/vm/cloud_hypervisor.rs`

Replace `krun.rs` with `cloud_hypervisor.rs`.

**`CloudHypervisorBackend`** struct:
```rust
struct CloudHypervisorBackend {
    ch_child: Option<Child>,        // cloud-hypervisor process
    virtiofsd_children: Vec<Child>, // virtiofsd processes
    vsock_socket_path: PathBuf,     // base vsock socket
}
```

**Start sequence:**
1. For each VirtioFS share: spawn virtiofsd process
   ```
   virtiofsd --socket-path=vfs-<tag>.sock --shared-dir=<host_path>
             --xattr --sandbox=none
             --translate-uid map:0:<host_uid>:1
             --translate-gid map:0:<host_gid>:1
   ```
   Wait for socket file to appear.

2. Spawn cloud-hypervisor:
   ```
   cloud-hypervisor
     --kernel <kernel_path>
     --initramfs <initramfs_path>
     --cmdline "console=hvc0 rdinit=/init ..."
     --cpus boot=<N>
     --memory size=<M>M,shared=on
     --console off --serial off
     --vsock cid=3,socket=vsock.sock
     --disk path=disk.img  (if cache)
     --fs tag=base,socket=vfs-base.sock,num_queues=1,queue_size=1024 ...
     --api-socket api.sock
   ```

3. Connect to vsock: `UnixStream::connect(vsock.sock_1024)`,
   send `CONNECT 1024\n`, read `OK` response.

**VmConfig changes:**
- `kernel` and `kernel_cmdline` become universal (not macOS-only)
- Add `cloud_hypervisor: PathBuf` and `virtiofsd: PathBuf`

**UID mapping:**
- virtiofsd `--translate-uid map:0:<host_uid>:1` maps guest root (0)
  to host user UID. Files created by root in the guest appear as the
  host user on the host filesystem.
- `--translate-gid map:0:<host_gid>:1` same for groups.
- `--sandbox none` since we're already in the host user's context.

### Phase 4: VmHandle trait

**Drop:** Kill all virtiofsd processes and cloud-hypervisor.
**wait_for_stop:** Monitor CH child process exit.

### Phase 5: Supervisor guest-side

The supervisor runs the same init::setup as before. With virtiofsd
providing proper xattr support, kernel overlayfs should work directly
(no fuse-overlayfs needed). Keep fuse-overlayfs as fallback initially.

### Phase 6: Cleanup

- Remove `cli/src/vm/krun.rs`
- Remove `sandbox/libkrun/` directory
- Remove `mise/tasks/build/libkrun`
- Remove KVM access check (CH handles this)
- Update CI to fetch CH + virtiofsd instead of building libkrun

## Files to modify

- `cli/src/vm/cloud_hypervisor.rs` — **new**, CH + virtiofsd backend
- `cli/src/vm/krun.rs` — **delete**
- `cli/src/vm.rs` — switch to cloud_hypervisor module
- `cli/src/vm/config.rs` — universal kernel/cmdline, add binary paths
- `cli/src/assets.rs` — embed CH + virtiofsd instead of libkrun
- `cli/build.rs` — checksum includes CH + virtiofsd + kernel + initramfs
- `mise/tasks/build/cloud-hypervisor` — **new**, fetch static binary
- `mise/tasks/build/virtiofsd` — **new**, fetch static binary
- `mise/tasks/build/kernel` — enable for Linux
- `mise/tasks/build/dev` — update deps
- `mise/tasks/build/libkrun` — **delete**
- `sandbox/libkrun/` — **delete** directory
- `cli/src/main.rs` — remove KVM check (CH handles it)

## Verification

1. `mise run build:kernel` — builds x86_64 kernel
2. `mise run build:cloud-hypervisor` — fetches CH binary
3. `mise run build:virtiofsd` — fetches virtiofsd binary
4. `mise run lint` — passes
5. `target/debug/ez -- echo test` — VM boots, runs command
6. `target/debug/ez -- ls /` — container rootfs works
7. `target/debug/ez` — interactive shell
8. Test with cache disk (ez.toml with cache config)
9. Test overlayfs (mkdir, file creation inside container)
10. Test networking (wget, DNS resolution)
