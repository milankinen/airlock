# Plan: Move init setup into supervisor, add block device support

## Context

The shell init script (`sandbox/rootfs/init`) does VM setup (mounts, networking,
cache disk) before starting the supervisor. Under libkrun, this causes issues:
the init script hangs or races with vsock connections when the cache disk is
attached. Moving the setup into the supervisor gives us proper error handling
via the RPC logging bridge, eliminates shell script fragility, and makes the
boot path consistent across platforms.

## High-level approach

1. Extend the RPC `start()` message with init config (shares, epoch, host_ports,
   cache disk flag)
2. Supervisor performs the init setup after connecting (mounts, networking, cache
   disk, DNS) with errors logged via LogSink RPC
3. Init script becomes a minimal passthrough: just exec supervisor
4. Build libkrun with BLK=1, add `krun_add_disk` FFI call
5. Test end-to-end with cache disk

## Detailed plan

### Phase 1: Extend RPC schema

**`protocol/schema/supervisor.capnp`** — add fields to `start()`:

```capnp
start @0 (
  ...existing params...
  shares :List(Text),        # virtiofs tags to mount at /mnt/<tag>
  epoch :UInt64,             # unix timestamp for clock sync
  hostPorts :List(UInt16),   # ports to redirect to proxy
  hasCacheDisk :Bool,        # whether /dev/vda should be formatted/mounted
) -> (proc :Process);
```

### Phase 2: Supervisor init setup

**`sandbox/supervisor/src/main.rs`** — after RPC connect and logging init,
before starting the process:

```rust
async fn run() {
    let conn = rpc::connect(conn_fd).await?;
    logging::init(conn.log_sink, &conn.log_filter);

    // NEW: perform init setup with error logging
    init::setup(&conn.init_config)?;

    // ...existing DNS, proxy, process spawn...
}
```

**`sandbox/supervisor/src/init.rs`** — new module:

```rust
pub struct InitConfig {
    pub shares: Vec<String>,
    pub epoch: u64,
    pub host_ports: Vec<u16>,
    pub has_cache_disk: bool,
    pub cache_dirs: Vec<String>,
}

pub fn setup(config: &InitConfig) -> anyhow::Result<()> {
    set_clock(config.epoch);
    mount_virtiofs(&config.shares)?;
    setup_networking(&config.host_ports)?;
    if config.has_cache_disk {
        setup_cache_disk(&config.cache_dirs)?;
    }
    setup_dns()?;
    Ok(())
}
```

Each function does what the init script currently does, but in Rust with
proper error handling. Uses `libc` for mount/ioctl calls, `std::process::Command`
for ip/iptables. All errors logged via tracing (goes to CLI via LogSink).

Key functions:
- `set_clock(epoch)` — `libc::clock_settime()` or `date -s`
- `mount_virtiofs(shares)` — `libc::mount()` for each tag at `/mnt/<tag>`
- `setup_networking(host_ports)` — ip link/addr/route + iptables via Command
- `setup_cache_disk(cache_dirs)` — check /dev/vda, mkfs if needed, mount, create subdirs
- `setup_dns()` — write resolv.conf to `/mnt/bundle/rootfs/etc/`

### Phase 3: CLI sends init config via RPC

**`cli/src/rpc/supervisor.rs`** — add init params to start request:

```rust
// In supervisor.start():
let tags: Vec<&str> = shares.iter().map(|s| s.tag.as_str()).collect();
req.get().set_shares(&tags);
req.get().set_epoch(epoch);
req.get().set_host_ports(&host_ports);
req.get().set_has_cache_disk(cache_disk.is_some());
```

The shares/epoch/host_ports data that was previously passed via env vars or
kernel cmdline is now sent directly over the RPC channel.

### Phase 4: Simplify init script

**`sandbox/rootfs/init`** — reduce to minimal:

```sh
#!/bin/sh
# Basic mounts (proc/sys/dev needed before supervisor can run)
mount -t proc none /proc 2>/dev/null
mount -t sysfs none /sys 2>/dev/null
mount -t devtmpfs none /dev 2>/dev/null
mkdir -p /dev/pts && mount -t devpts none /dev/pts 2>/dev/null
mount -t tmpfs none /tmp 2>/dev/null
mount -t tmpfs none /run 2>/dev/null
mkdir -p /sys/fs/cgroup && mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null
hostname ezpez
ip link set lo up 2>/dev/null
/usr/bin/supervisor
kill -TERM -1 2>/dev/null
sleep 0.2
/sbin/poweroff -f 2>/dev/null
```

All virtiofs mounts, networking config, cache disk, DNS — moved to supervisor.

### Phase 5: Build libkrun with BLK=1

**`sandbox/libkrun/build.sh`** — add `BLK=1` to make and `libclang-dev`+
`libcap-ng-dev` to apt deps.

**`cli/src/vm/krun.rs`** — add `add_disk` FFI function pointer:

```rust
add_disk: unsafe extern "C" fn(
    ctx_id: u32, block_id: *const c_char, disk_path: *const c_char, read_only: bool
) -> i32,
```

Call it when `config.cache_disk.is_some()`.

### Phase 6: Remove env var passing

Remove `build_env()` from krun.rs and the `EZPEZ_*` env var handling from
the init script. The init script no longer needs config — supervisor gets it
all via RPC.

## Files to modify

- `protocol/schema/supervisor.capnp` — add init config fields to start()
- `sandbox/supervisor/src/init.rs` — **new file**, VM init setup
- `sandbox/supervisor/src/main.rs` — call init::setup() after RPC connect
- `sandbox/supervisor/src/rpc.rs` — extract new fields from start() params
- `sandbox/rootfs/init` — simplify to minimal mounts + exec supervisor
- `cli/src/rpc/supervisor.rs` — pass init config in start() call
- `cli/src/vm/krun.rs` — add add_disk FFI, remove build_env()
- `cli/src/vm.rs` — pass shares/epoch/host_ports to supervisor start
- `sandbox/libkrun/build.sh` — add BLK=1, libclang-dev, libcap-ng-dev

## Verification

1. `mise run lint` — must pass
2. `mise run build:libkrun` — builds with BLK support
3. `target/debug/ez -- echo test` — basic command (no cache)
4. `target/debug/ez -- echo test` (from project with cache config) — cache disk works
5. `target/debug/ez` — interactive shell works
6. Check `~/.ezpez/projects/*/ez.log` for init setup logs from supervisor
