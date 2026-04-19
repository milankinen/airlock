//! Set the guest system clock from the host-provided wall time. VMs
//! have no RTC, so until we call this any `time(2)` inside the sandbox
//! returns kernel boot epoch + seconds — enough to break TLS cert
//! validation and every `mtime`-driven build tool.

use tracing::{debug, warn};

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
