//! Guest stats collector for `Supervisor.pollStats` RPC.
//!
//! Samples `/proc/stat` (per-core CPU), `/proc/meminfo` (total/available),
//! and `/proc/loadavg`. Per-core utilization requires two samples, so
//! [`Collector`] keeps the previous `/proc/stat` snapshot across calls.
//! The first call returns zero-filled per-core values.

use std::fs;

/// A single `/proc/stat` sample: per-core (idle, total) jiffies.
#[derive(Default, Clone)]
struct CpuSample {
    per_core: Vec<(u64, u64)>,
}

/// Result of a single poll.
pub struct Snapshot {
    pub per_core: Vec<u8>,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub load_avg: (f32, f32, f32),
}

/// Stateful collector. One instance per supervisor.
#[derive(Default)]
pub struct Collector {
    prev: Option<CpuSample>,
}

impl Collector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sample all sources. Returns zeros for per-core on the very first call.
    pub fn poll(&mut self) -> Snapshot {
        let cur = read_cpu_sample();
        let per_core = match self.prev.as_ref() {
            Some(prev) if prev.per_core.len() == cur.per_core.len() => diff_per_core(prev, &cur),
            _ => vec![0u8; cur.per_core.len()],
        };
        self.prev = Some(cur);

        let (total_bytes, used_bytes) = read_memory().unwrap_or((0, 0));
        let load_avg = read_loadavg().unwrap_or((0.0, 0.0, 0.0));

        Snapshot {
            per_core,
            total_bytes,
            used_bytes,
            load_avg,
        }
    }
}

/// Parse `/proc/stat` per-core lines. Returns zero-length vec on read error.
fn read_cpu_sample() -> CpuSample {
    let Ok(data) = fs::read_to_string("/proc/stat") else {
        return CpuSample::default();
    };
    let mut per_core = Vec::new();
    for line in data.lines() {
        // per-core lines start with "cpuN " (N digit). Skip the aggregate "cpu " line.
        if !line.starts_with("cpu") {
            break;
        }
        let mut it = line.split_ascii_whitespace();
        let Some(tag) = it.next() else { continue };
        if tag == "cpu" || !tag.starts_with("cpu") {
            continue;
        }
        let fields: Vec<u64> = it.filter_map(|s| s.parse::<u64>().ok()).collect();
        // user, nice, system, idle, iowait, irq, softirq, steal, ...
        if fields.len() < 4 {
            continue;
        }
        let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
        let total: u64 = fields.iter().sum();
        per_core.push((idle, total));
    }
    CpuSample { per_core }
}

fn diff_per_core(prev: &CpuSample, cur: &CpuSample) -> Vec<u8> {
    prev.per_core
        .iter()
        .zip(cur.per_core.iter())
        .map(|(&(pi, pt), &(ci, ct))| {
            let di = ci.saturating_sub(pi);
            let dt = ct.saturating_sub(pt);
            if dt == 0 {
                0
            } else {
                let busy = dt.saturating_sub(di);
                u8::try_from((busy * 100) / dt).unwrap_or(0).min(100)
            }
        })
        .collect()
}

/// Parse `/proc/meminfo` → `(total_bytes, used_bytes)` where `used =
/// total - available`. Fields are reported in kB.
fn read_memory() -> Option<(u64, u64)> {
    let data = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total_kb: Option<u64> = None;
    let mut avail_kb: Option<u64> = None;
    for line in data.lines() {
        let (key, rest) = line.split_once(':')?;
        let value_kb: u64 = rest.split_ascii_whitespace().next()?.parse().ok()?;
        match key {
            "MemTotal" => total_kb = Some(value_kb),
            "MemAvailable" => avail_kb = Some(value_kb),
            _ => {}
        }
        if total_kb.is_some() && avail_kb.is_some() {
            break;
        }
    }
    let total = total_kb? * 1024;
    let avail = avail_kb? * 1024;
    Some((total, total.saturating_sub(avail)))
}

fn read_loadavg() -> Option<(f32, f32, f32)> {
    let data = fs::read_to_string("/proc/loadavg").ok()?;
    let mut it = data.split_ascii_whitespace();
    let one = it.next()?.parse().ok()?;
    let five = it.next()?.parse().ok()?;
    let fifteen = it.next()?.parse().ok()?;
    Some((one, five, fifteen))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_per_core_computes_percent() {
        let prev = CpuSample {
            per_core: vec![(100, 1000), (200, 1000)],
        };
        let cur = CpuSample {
            per_core: vec![(150, 1100), (200, 1100)],
        };
        // core 0: dt=100, di=50, busy=50, => 50%
        // core 1: dt=100, di=0, busy=100 => 100%
        assert_eq!(diff_per_core(&prev, &cur), vec![50, 100]);
    }

    #[test]
    fn diff_per_core_zero_when_no_change() {
        let prev = CpuSample {
            per_core: vec![(100, 1000)],
        };
        let cur = prev.clone();
        assert_eq!(diff_per_core(&prev, &cur), vec![0]);
    }

    #[test]
    fn collector_first_call_returns_zeros() {
        let mut c = Collector::new();
        let snap = c.poll();
        // Regardless of core count, first call must be all-zero.
        assert!(snap.per_core.iter().all(|&v| v == 0));
    }
}
