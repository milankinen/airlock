# Host-driven memory balloon reclaim + host memory in the monitor

## Context

`airlock` attaches a `VZVirtioTraditionalMemoryBalloonDeviceConfiguration` to
the VM (`app/airlock-cli/src/vm/apple.rs:211-215`) but never drives it. On
Apple's Virtualization.framework the balloon is entirely host-controlled: the
framework only releases guest pages back to macOS after the host lowers
`targetVirtualMachineMemorySize` on the live
`VZVirtioTraditionalMemoryBalloonDevice`. Nothing in the codebase reads
`memoryBalloonDevices` or sets the target, and the objc2 feature that exposes
those runtime classes (`VZMemoryBalloonDevice`) is not enabled in
`app/airlock-cli/Cargo.toml`. Result: after a memory-hungry job inside the
sandbox finishes, the airlock process keeps its peak footprint forever. The
manual already lists the balloon as "Future: reclaim unused guest memory"
(`docs/manual/src/technical/virtualization.md:50`).

Decisions made with the user:

- **Reclaim freed guest pages only, never hold the guest shrunk.** macOS has
  no host-only way to find free guest pages and VZ offers only the
  traditional balloon, so the host emulates Linux's *free page reporting*
  with a short balloon "pump": inflate by the guest's free pages, let the host
  discard them, deflate again. Page cache is left alone.
- **Linux (Cloud Hypervisor) uses real free page reporting**
  (`free_page_reporting=on`, plus `deflate_on_oom=on`); no host controller.
- **Monitor shows the host footprint**: status line
  `Memory <host> (<guest>) / <total>` and the memory widget's `used` row.

Guest kernel side needs nothing: `CONFIG_VIRTIO_BALLOON=y` on both arches,
and `VIRTIO_BALLOON` selects `PAGE_REPORTING` (the arm64 config is a fragment
expanded by `make olddefconfig`, `app/vm-kernel/build.sh:11`).

## Cost model (why a pump, why these bounds)

- The 10 s check is one `pollStats` RPC + one local syscall: negligible.
- A pump costs ~seconds per few GiB on one guest vCPU (balloon driver
  allocates free pages in 256-page batches) plus host-side discard; nothing
  freezes, other vCPUs and the airlock main thread keep running.
- The hidden cost is refaulting: pages handed back come back untouched and
  cost a zero-fill fault when next written (~0.5 s/GiB). Same trade as free
  page reporting on Linux. Hence: minimum gap, maximum pump size, rate limit,
  and backoff when a pump frees nothing.
- A pump only takes `MemFree - margin`, so it never evicts page cache and the
  guest keeps `margin` + all reclaimable cache during the few-second window.

## Pump policy (pure, unit-tested: `app/airlock-cli/src/vm/balloon.rs`)

Sample per tick (`TICK = 10 s`):

| symbol       | source                                                          |
|--------------|-----------------------------------------------------------------|
| `configured` | `[vm] memory`                                                   |
| `total`      | guest `MemTotal` (`pollStats.total_bytes`)                      |
| `free`       | guest `MemFree` (**new** `pollStats.free_bytes`)                |
| `host`       | host footprint of the VM (see below)                            |

Derived (all saturating, all rounded down to 1 MiB — VZ requires MiB targets):

- `non_free = total - free`
- `gap = host - non_free` — what the host holds for pages the guest thinks are
  free (includes airlock's own heap, tens of MiB, covered by `MIN_GAP`).
- `margin = max(MARGIN_MIN = 256 MiB, free / 8)`
- `size = min(free - margin, gap + gap / 4, MAX_PUMP = 2 GiB)`

`fn plan_pump(&Policy, &State, Sample, now) -> Option<u64 /* size */>` returns
`Some(size)` iff:

1. balloon at rest (`target == configured`) and `free > 0` (old guest
   binaries report 0 → never pump),
2. `gap >= MIN_GAP = 512 MiB` and `size >= MIN_GAP`,
3. `now - last_pump >= wait`, where `wait` starts at `PUMP_INTERVAL = 60 s`
   and doubles after each *ineffective* pump (freed < 10 % of size), capped
   at `10 min`; an effective pump resets it.

Constants live in one `Policy` struct so tests can shrink them.

Pump execution (`async fn pump(handle, supervisor, size)`), always restores the
target even on error (guard / `finally`-style):

1. `host_before = host_memory_bytes()`; `set_memory_target(configured - size)`.
2. Poll `host_memory_bytes()` every 250 ms until
   `host_before - host_now >= size * 9 / 10` or timeout
   `2 s + 2 s per GiB of size` (so a lazily-freeing host can't wedge the
   guest in the shrunken state).
3. `set_memory_target(configured)`.
4. `info!` one line: requested size, host bytes freed, elapsed; feed
   `freed` back into `State` for the backoff.

Task lifecycle: `VmInstance::spawn_balloon(&mut self, supervisor)` spawns the
loop with `spawn_local` (the whole run is a `current_thread` runtime inside a
`LocalSet`, `main.rs:30,67`), skipping the immediate first tick like
`spawn_clock_sync` (`rpc/supervisor.rs:128-135`), using `tokio::time::sleep`
per iteration. The task holds a `Weak<dyn VmHandle>` and upgrades per tick
(exits when the VM is gone). `VmInstance` stores the `JoinHandle`;
`shutdown()` does `abort()` **then `await`s the handle** before dropping the
VM (abort alone is asynchronous and would keep the `Rc` alive past teardown);
`Drop` aborts as a fallback — same shape as `SyncHandle`
(`vm/file_sync.rs:64-88`).

## Host footprint

- **macOS**: `libc::proc_pid_rusage(getpid(), RUSAGE_INFO_V4, ..)` →
  `rusage_info_v4.ri_phys_footprint` (libc 0.2.189 pinned in `Cargo.lock`,
  bindings verified). Guest RAM lives inside the airlock process, so this is
  the number Activity Monitor shows.
- **Linux**: `VmRSS` from `/proc/<cloud-hypervisor pid>/status`
  (`ch_child.id()`, `vm/cloud_hypervisor.rs:19`).

## Implementation plan

### Phase 1 — guest reports `MemFree`

- `app/airlock-common/schema/supervisor.capnp:188-191`: `MemoryStats` gains
  `freeBytes @2 :UInt64` (appending is wire-compatible).
- `app/airlockd/src/stats.rs`: `Snapshot.free_bytes`; `read_memory` also
  parses `MemFree` and returns `(total, used, free)`; extend its tests.
- `app/airlockd/src/rpc.rs:405-436`: `mem.set_free_bytes(..)`.
- `app/airlock-cli/src/rpc/supervisor.rs:17-24,363-381`: `StatsSnapshot.free_bytes`.

### Phase 2 — policy module

`app/airlock-cli/src/vm/balloon.rs` (new): `Policy`, `State`, `Sample`,
`plan_pump`, `record_result(freed)`, and the async `pump` + `run` loop.
Unit tests (policy only, no I/O): no pump when gap small; no pump when
`free == 0`; size bounded by `free - margin`, `gap * 1.25`, `MAX_PUMP`, and
MiB-rounded; rate limit and backoff doubling/capping/reset; nothing while
not at rest; `host == None` → never pump.

### Phase 3 — backend plumbing (`vm.rs`, `vm/apple.rs`, `vm/cloud_hypervisor.rs`)

- Trait `VmHandle` (`vm.rs:432`) gains
  - `fn host_memory_bytes(&self) -> Option<u64>`
  - `fn set_memory_target(&self, bytes: u64) -> Option<Pin<Box<dyn Future<Output = anyhow::Result<()>> + '_>>>`
    (`None` = backend has no host-driven balloon; Linux returns `None`).
- `AppleVmBackend::set_memory_target`: copy the dispatch-queue pattern of
  `vsock_connect` (`apple.rs:412-480`): load the `AtomicPtr`, bail on null,
  `vm.memoryBalloonDevices().firstObject_unchecked()`, pointer-cast
  `VZMemoryBalloonDevice → VZVirtioTraditionalMemoryBalloonDevice` (exactly one
  traditional balloon is configured), `setTargetVirtualMachineMemorySize(bytes)`
  (synchronous, no completion handler), deliver `Ok(())`; wrap in `catch_obj`.
  Enable Cargo feature **`VZMemoryBalloonDevice`** (this is the gate for the
  struct, its impl, and `memoryBalloonDevices`; the identically named
  `VZVirtioTraditionalMemoryBalloonDevice` feature is empty) in
  `app/airlock-cli/Cargo.toml:84`.
- `AppleVmBackend::host_memory_bytes`: `proc_pid_rusage` as above.
- `CloudHypervisorBackend`: command line gains
  `--balloon size=0,deflate_on_oom=on,free_page_reporting=on`
  (`cloud_hypervisor.rs:87-105`; bundled cloud-hypervisor is v51.1, which
  supports these); `host_memory_bytes` reads `VmRSS`; `set_memory_target`
  returns `None`.
- `VmInstance` (`vm.rs:87`): `vm_handle: Rc<dyn VmHandle>` (`boot_backend`
  returns `Rc<dyn VmHandle>`; no `Send` bound exists anywhere on this path —
  verified), `balloon: Option<JoinHandle<()>>`, `pub fn memory_probe(&self)
  -> MemoryProbe` (pub struct, private `Weak<dyn VmHandle>`, `fn host_bytes`),
  `pub fn spawn_balloon(&mut self, supervisor: rpc::Supervisor)` (no-op when
  `set_memory_target` is `None`), shutdown/Drop as described above.

### Phase 4 — wiring + config

- `config::VirtualMachine` (`config.rs:238-263`): `#[config(default_t = true)]
  pub balloon: bool` next to `harden`, doc: "Return memory the sandbox has
  freed to the host (virtio memory balloon)". No schema fixtures enumerate
  fields (verified).
- `cli/cmd_start.rs:200-215`: `let (mut vm, vsock_fd) = ..`; after
  `Supervisor::connect`, `if project.config.vm.balloon { vm.spawn_balloon(supervisor.clone()) }`.
- `runtime::Runtime::launch` (`runtime.rs:59-64`, implementors
  `runtime/raw_terminal.rs:97`, `runtime/monitor_terminal.rs:73`) gains a
  `vm::MemoryProbe` parameter; the monitor's 1 s loop
  (`monitor_terminal.rs:112-130`, the only constructor of
  `airlock_monitor::StatsSnapshot`) fills `host_used_bytes: probe.host_bytes()`.

### Phase 5 — monitor UI (`app/airlock-monitor`)

- `lib.rs:35 StatsSnapshot`: `pub host_used_bytes: Option<u64>`.
- `tabs/monitor/memory.rs`: `MemoryState.host_used_bytes`,
  `set_usage(total, used, host)`, `pub fn used_label(&self) -> String`
  (`"<host> (<guest>)"` or `"<guest>"`), make `format_bytes` `pub(crate)` and
  delete the duplicate in `ui.rs:221`. Widget `used` row uses `used_label`;
  sparkline stays guest used%.
- `ui.rs:189-210 build_status_line`: `Memory {used_label} / {total}`.
- `tabs/monitor/mod.rs:64-69 apply_stats`: pass the host figure.
- Tests: `used_label` both forms; existing `set_usage` test updated.

### Phase 6 — docs

- `docs/manual/src/configuration/vm.md` "Resources": new "Memory reclaim"
  subsection: what the pump does, the `balloon = true` knob, that guest
  `MemTotal`/`used` figures can look odd for a few seconds during a pump.
- `docs/manual/src/technical/virtualization.md:50`: replace "Future: …" with
  the actual behaviour; mention Linux uses free page reporting.
- `docs/manual/src/usage/monitor.md:82`: document `used: <host> (<guest>)`
  in the widget and the status line.
- `docs/log/2026-09-15-memory-balloon-reclaim.md` (new file, existing format).
- Copy this plan to `docs/plans/2026-09-15-memory-balloon-reclaim.md`.

## Verification

This session runs on Linux without KVM, so the macOS backend cannot be run
here. Verified here:

1. `mise run test` — policy, stats parser, monitor tests.
2. `mise run lint`, `mise format`.
3. `mise x -- cargo check --target aarch64-apple-darwin -p airlock-cli` —
   target and crates are cached locally, so the macOS code path is type-checked.
4. `mise run build:airlockd` if the schema change needs the guest binary
   rebuilt for the bundled assets (check how `Assets` picks up airlockd).

On the user's Mac (`mise dev`, then inside the sandbox):

```sh
python3 -c "b=bytearray(3<<30); input()"   # hold ~3 GiB, Enter to free
```

Expect: status line host figure ≳3 GiB while held; within ~10–70 s after
freeing (10 s tick + 60 s rate limit at most) a log line
`balloon pump: requested … freed … in …` and the host figure / Activity
Monitor drop back near the guest figure. Note the elapsed time and freed
ratio: they tune `MAX_PUMP`, the timeout, and confirm VZ frees eagerly.
Then rerun the allocation: it must succeed with no OOM and no visible stall.

Also record which balloon features VZ negotiates (device id 5; `features` is a
`0`/`1` string with bit *i* at index *i*; bit 2 = `DEFLATE_ON_OOM`, bit 5 =
`REPORTING`):

```sh
for d in /sys/bus/virtio/devices/*; do
  [ "$(cat $d/device)" = 0x0005 ] && cat $d/features
done
```

Linux (KVM host, not this session): `VmRSS` of `cloud-hypervisor` should
drop by itself a few seconds after the allocation exits (free page reporting).
