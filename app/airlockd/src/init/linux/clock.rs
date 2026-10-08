//! Guest system clock.
//!
//! Sets the guest clock to the wall time from the host. The VM has no hardware
//! clock of its own.

use tracing::{debug, warn};

/// Set `CLOCK_REALTIME` to the given wall time. Does nothing if `epoch` is
/// 0. A failure only logs a warning.
///
/// VMs have no RTC. Until this call, `time(2)` in the sandbox returns the
/// kernel boot epoch plus uptime. This breaks TLS cert validation and all
/// build tools that use `mtime`.
pub(super) fn set(epoch: u64, epoch_nanos: u32) {
    if epoch == 0 {
        return;
    }
    let ts = libc::timespec {
        tv_sec: epoch as i64,
        tv_nsec: i64::from(epoch_nanos),
    };
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &raw const ts) } != 0 {
        warn!("failed to set system clock");
    } else {
        debug!("system clock set to epoch {epoch}.{epoch_nanos:09}");
    }
}
