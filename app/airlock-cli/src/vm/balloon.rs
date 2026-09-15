//! Host-driven memory balloon "pump" — hands pages the guest has freed
//! back to the host without leaving the guest shrunk.
//!
//! Apple's Virtualization.framework exposes only the traditional virtio
//! balloon and never drives it: guest memory is released to macOS only
//! after the host lowers the balloon's target memory size, the guest
//! inflates the balloon with free pages, and the framework discards them.
//! Holding the balloon inflated would shrink the guest indefinitely and
//! risk OOM inside it, so instead this module emulates Linux's free page
//! reporting: every tick compare the host footprint of the VM with what
//! the guest considers non-free; when the host holds much more, inflate
//! the balloon by (part of) `MemFree`, wait for the footprint to drop,
//! then deflate again. The pages come back to the guest untouched and
//! cost the host nothing until they are written again.
//!
//! Only `MemFree` is ever taken, so a pump never evicts guest page cache,
//! and a margin plus the reclaimable cache stay available to the guest
//! for the few seconds the balloon is inflated.
//!
//! The policy is pure ([`Policy`], [`State`]) so it can be unit-tested;
//! [`run`] is the async loop that wires it to a VM backend.

use std::rc::Weak;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use super::VmHandle;
use crate::rpc::Supervisor;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Tunables for the pump. One instance per VM; tests shrink the numbers.
#[derive(Debug, Clone)]
pub struct Policy {
    /// `[vm] memory` — the balloon target at rest.
    pub configured: u64,
    /// How often the host footprint is compared with guest usage.
    pub tick: Duration,
    /// The host must hold at least this much beyond the guest's non-free
    /// memory before a pump is worth its refault cost. Also the minimum
    /// pump size.
    pub min_gap: u64,
    /// Free memory always left to the guest while the balloon is
    /// inflated (the effective margin is `max(margin_min, free / 8)`).
    pub margin_min: u64,
    /// Upper bound per pump so a single cycle never occupies a guest
    /// vCPU for long; a larger gap is drained over several cycles.
    pub max_pump: u64,
    /// Minimum spacing between pumps.
    pub interval: Duration,
    /// Spacing doubles after every ineffective pump, up to this.
    pub max_interval: Duration,
}

impl Policy {
    pub fn new(configured: u64) -> Self {
        Self {
            configured,
            tick: Duration::from_secs(10),
            min_gap: 512 * MIB,
            margin_min: 256 * MIB,
            max_pump: 2 * GIB,
            interval: Duration::from_mins(1),
            max_interval: Duration::from_mins(10),
        }
    }

    /// How long a pump may keep the balloon inflated while waiting for
    /// the host footprint to drop: a fixed 2 s plus 2 s per GiB.
    pub fn pump_timeout(size: u64) -> Duration {
        Duration::from_secs(2) + Duration::from_millis(2000 * size / GIB)
    }
}

/// One tick's measurements.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// Guest `MemTotal`.
    pub total: u64,
    /// Guest `MemFree` (zero when the guest predates the field — never
    /// pump on a guess).
    pub free: u64,
    /// Host-side footprint of the VM, `None` if the backend can't tell.
    pub host: Option<u64>,
}

/// Rate-limit state across pumps. The balloon itself is always at rest
/// between pumps because [`run`] restores the target before returning
/// to the tick loop.
#[derive(Debug)]
pub struct State {
    last_pump: Option<Instant>,
    wait: Duration,
}

impl State {
    pub fn new(policy: &Policy) -> Self {
        Self {
            last_pump: None,
            wait: policy.interval,
        }
    }

    /// Decide whether to pump now and by how much (bytes, a multiple of
    /// 1 MiB as Virtualization.framework requires).
    pub fn plan_pump(&self, policy: &Policy, sample: Sample, now: Instant) -> Option<u64> {
        let host = sample.host?;
        if sample.free == 0 {
            return None;
        }
        if let Some(last) = self.last_pump
            && now.duration_since(last) < self.wait
        {
            return None;
        }
        // What the host holds for pages the guest thinks are free. Also
        // covers airlock's own heap (tens of MiB), hence `min_gap`.
        let non_free = sample.total.saturating_sub(sample.free);
        let gap = host.saturating_sub(non_free);
        if gap < policy.min_gap {
            return None;
        }
        let margin = policy.margin_min.max(sample.free / 8);
        // The guest hands over recently freed pages first (buddy free
        // lists are LIFO), so a little more than `gap` is enough; the
        // rest of `free` is likely untouched and would be wasted work.
        let size = sample
            .free
            .saturating_sub(margin)
            .min(gap.saturating_add(gap / 4))
            .min(policy.max_pump)
            / MIB
            * MIB;
        (size >= policy.min_gap).then_some(size)
    }

    /// Record a finished pump. A pump that freed less than 10 % of what it
    /// asked for doubles the wait before the next one (the guest handed
    /// over pages the host wasn't backing, or the host frees lazily); an
    /// effective one resets it.
    pub fn record(&mut self, policy: &Policy, size: u64, freed: u64, now: Instant) {
        self.last_pump = Some(now);
        self.wait = if freed.saturating_mul(10) < size {
            (self.wait * 2).min(policy.max_interval)
        } else {
            policy.interval
        };
    }
}

/// Tick loop: sample, decide, pump. Exits when the VM handle is gone.
/// Holds a strong reference to the backend only for the duration of a
/// tick so it never delays VM teardown.
pub(super) async fn run(policy: Policy, vm: Weak<dyn VmHandle>, supervisor: Supervisor) {
    let mut state = State::new(&policy);
    loop {
        tokio::time::sleep(policy.tick).await;
        let Some(vm) = vm.upgrade() else { break };
        let snap = match supervisor.poll_stats().await {
            Ok(snap) => snap,
            Err(e) => {
                debug!("balloon: poll_stats failed: {e}");
                continue;
            }
        };
        let sample = Sample {
            total: snap.total_bytes,
            free: snap.free_bytes,
            host: vm.host_memory_bytes(),
        };
        let Some(size) = state.plan_pump(&policy, sample, Instant::now()) else {
            continue;
        };
        let freed = match pump(&*vm, &policy, size).await {
            Ok(freed) => freed,
            Err(e) => {
                warn!("balloon pump failed: {e}");
                0
            }
        };
        state.record(&policy, size, freed, Instant::now());
    }
}

async fn set_target(vm: &dyn VmHandle, bytes: u64) -> anyhow::Result<()> {
    vm.set_memory_target(bytes)
        .ok_or_else(|| anyhow::anyhow!("backend has no host-driven balloon"))?
        .await
}

/// Inflate the balloon by `size`, wait until the host footprint has
/// dropped by (most of) that or the timeout passes, then deflate. Returns
/// the host bytes freed. The target is restored on every path after the
/// first successful inflate.
async fn pump(vm: &dyn VmHandle, policy: &Policy, size: u64) -> anyhow::Result<u64> {
    let started = Instant::now();
    let before = vm.host_memory_bytes().unwrap_or(0);
    set_target(vm, policy.configured - size).await?;

    let deadline = started + Policy::pump_timeout(size);
    let freed = loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let freed = before.saturating_sub(vm.host_memory_bytes().unwrap_or(before));
        if freed >= size / 10 * 9 || Instant::now() >= deadline {
            break freed;
        }
    };

    let restored = set_target(vm, policy.configured).await;
    info!(
        requested_mib = size / MIB,
        freed_mib = freed / MIB,
        elapsed_ms = started.elapsed().as_millis(),
        "balloon pump"
    );
    restored?;
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::new(8 * GIB)
    }

    fn sample(total: u64, free: u64, host: u64) -> Sample {
        Sample {
            total,
            free,
            host: Some(host),
        }
    }

    #[test]
    fn no_pump_when_host_holds_little_beyond_guest_usage() {
        let p = policy();
        let s = State::new(&p);
        // 2 GiB non-free, host holds 2.25 GiB: gap 256 MiB < min_gap.
        let smp = sample(8 * GIB, 6 * GIB, 2 * GIB + 256 * MIB);
        assert_eq!(s.plan_pump(&p, smp, Instant::now()), None);
    }

    #[test]
    fn pump_size_tracks_gap_with_slack() {
        let p = policy();
        let s = State::new(&p);
        // 2 GiB non-free, host holds 3 GiB: gap 1 GiB → 1.25 GiB.
        let smp = sample(8 * GIB, 6 * GIB, 3 * GIB);
        assert_eq!(s.plan_pump(&p, smp, Instant::now()), Some(GIB + 256 * MIB));
    }

    #[test]
    fn pump_size_is_capped_by_max_pump() {
        let p = policy();
        let s = State::new(&p);
        // gap 5 GiB, free 7 GiB → capped at 2 GiB.
        let smp = sample(8 * GIB, 7 * GIB, 6 * GIB);
        assert_eq!(s.plan_pump(&p, smp, Instant::now()), Some(2 * GIB));
    }

    #[test]
    fn pump_size_leaves_margin_of_free() {
        let p = policy();
        let s = State::new(&p);
        // free 1 GiB → margin 256 MiB → at most 768 MiB even though gap is big.
        let smp = sample(8 * GIB, GIB, 8 * GIB);
        assert_eq!(s.plan_pump(&p, smp, Instant::now()), Some(768 * MIB));
        // free 4 GiB → margin 512 MiB (free / 8).
        let smp = sample(8 * GIB, 4 * GIB, 8 * GIB);
        assert_eq!(s.plan_pump(&p, smp, Instant::now()), Some(2 * GIB));
    }

    #[test]
    fn no_pump_when_free_after_margin_is_below_min_gap() {
        let p = policy();
        let s = State::new(&p);
        // free 600 MiB → 344 MiB after margin < 512 MiB.
        let smp = sample(8 * GIB, 600 * MIB, 8 * GIB);
        assert_eq!(s.plan_pump(&p, smp, Instant::now()), None);
    }

    #[test]
    fn size_is_rounded_down_to_mib() {
        let p = policy();
        let s = State::new(&p);
        let gap = GIB + 12345;
        let smp = sample(8 * GIB, 6 * GIB, 2 * GIB + gap);
        let size = s.plan_pump(&p, smp, Instant::now()).unwrap();
        assert_eq!(size % MIB, 0);
        assert!(size <= gap + gap / 4);
    }

    #[test]
    fn no_pump_without_host_figure_or_guest_free() {
        let p = policy();
        let s = State::new(&p);
        let now = Instant::now();
        let mut smp = sample(8 * GIB, 6 * GIB, 8 * GIB);
        smp.host = None;
        assert_eq!(s.plan_pump(&p, smp, now), None);
        assert_eq!(s.plan_pump(&p, sample(8 * GIB, 0, 8 * GIB), now), None);
    }

    #[test]
    fn rate_limit_and_backoff() {
        let p = policy();
        let mut s = State::new(&p);
        let t0 = Instant::now();
        let smp = sample(8 * GIB, 6 * GIB, 8 * GIB);
        let size = s.plan_pump(&p, smp, t0).unwrap();

        // Effective pump: next one allowed after `interval`.
        s.record(&p, size, size, t0);
        assert_eq!(s.plan_pump(&p, smp, t0 + Duration::from_secs(59)), None);
        assert!(s.plan_pump(&p, smp, t0 + Duration::from_mins(1)).is_some());

        // Ineffective pumps double the wait, capped at max_interval.
        s.record(&p, size, 0, t0);
        assert_eq!(s.wait, Duration::from_mins(2));
        s.record(&p, size, size / 20, t0);
        assert_eq!(s.wait, Duration::from_mins(4));
        s.record(&p, size, 0, t0);
        s.record(&p, size, 0, t0);
        s.record(&p, size, 0, t0);
        assert_eq!(s.wait, p.max_interval);
        assert_eq!(s.plan_pump(&p, smp, t0 + Duration::from_secs(599)), None);
        assert!(s.plan_pump(&p, smp, t0 + Duration::from_mins(10)).is_some());

        // An effective pump resets the wait.
        s.record(&p, size, size / 5, t0);
        assert_eq!(s.wait, p.interval);
    }

    #[test]
    fn pump_timeout_scales_with_size() {
        assert_eq!(Policy::pump_timeout(0), Duration::from_secs(2));
        assert_eq!(Policy::pump_timeout(2 * GIB), Duration::from_secs(6));
    }
}
