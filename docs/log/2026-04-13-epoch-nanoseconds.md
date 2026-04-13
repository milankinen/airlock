# Add nanosecond precision to synced guest clock

VMs have no RTC, so the host provides the current wall-clock time via the
`Supervisor.start()` RPC. Previously only seconds were transmitted
(`epoch :UInt64`), leaving `tv_nsec = 0` in the guest `clock_settime` call.

## Change

Added `epochNanos :UInt32` to the `Supervisor.start` Cap'n Proto message.
The host captures `SystemTime::now().subsec_nanos()` alongside the existing
seconds value and sends both fields. The guest sets `tv_nsec = epoch_nanos`
in `clock_settime(CLOCK_REALTIME, ...)`.

Pipeline touched:
- `supervisor.capnp` — new `epochNanos :UInt32` field
- `cmd_up.rs` — capture `Duration::subsec_nanos()` from `SystemTime::now()`
- `rpc/supervisor.rs` (host) — send `set_epoch_nanos(epoch_nanos)`
- `init.rs` (guest) — `InitConfig` gains `epoch_nanos: u32`
- `rpc.rs` (guest) — read `params.get_epoch_nanos()`
- `init/linux.rs` — `set_clock(epoch, epoch_nanos)` sets `tv_nsec`

The kernel cmdline `airlock.epoch` is not parsed by the guest (clock sync
happens entirely via RPC), so it was left as seconds-only.
