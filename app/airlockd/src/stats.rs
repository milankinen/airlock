//! Guest resource stats.
//!
//! Measures the CPU use of each core, the memory use and the load average of
//! the guest. The host polls these stats to show the resource use of the
//! sandbox.

use std::fs;

/// A single `/proc/stat` sample: per-core (idle, total) jiffies.
#[derive(Default, Clone)]
struct CpuSample {
    per_core: Vec<(u64, u64)>,
}

/// Result of a single poll.
pub struct Snapshot {
    /// CPU use of each core since the previous poll, in percent (0-100).
    pub per_core: Vec<u8>,
    /// Total memory in bytes.
    pub total_bytes: u64,
    /// Used memory in bytes (total minus available).
    pub used_bytes: u64,
    /// Load average over 1, 5 and 15 minutes.
    pub load_avg: (f32, f32, f32),
}

/// Stats collector with state. One instance per supervisor.
///
/// Per-core CPU use needs two samples, so the collector keeps the previous
/// `/proc/stat` sample between calls.
#[derive(Default)]
pub struct Collector {
    prev: Option<CpuSample>,
}

impl Collector {
    /// Create a collector with no previous sample.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read all sources. The first call returns zeros for per-core CPU use.
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

/// Parse the per-core lines of `/proc/stat`. Returns an empty sample on a
/// read error.
fn read_cpu_sample() -> CpuSample {
    let Ok(data) = fs::read_to_string("/proc/stat") else {
        return CpuSample::default();
    };
    let mut per_core = Vec::new();
    for line in data.lines() {
        // Per-core lines start with "cpuN " (N is a number). Skip the total
        // "cpu " line.
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

/// Calculate the CPU use of each core between two samples, in percent.
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

/// Parse `/proc/meminfo` into `(total_bytes, used_bytes)`, where
/// `used = total - available`. `/proc/meminfo` gives the values in kB.
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

/// Parse the 1, 5 and 15 minute load averages from `/proc/loadavg`.
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
    //! Tests of the CPU and memory statistics of the guest.

    use super::*;

    /// Test that the CPU use of a core is the busy share of the elapsed
    /// jiffies. The monitor shows this value per core.
    ///   1. Make two samples with a half busy, a fully busy and an idle core
    ///   2. Check that the result is 50, 100 and 0 percent
    #[test]
    fn diff_per_core_is_busy_share_of_elapsed_jiffies() {
        let prev = CpuSample {
            // Each tuple is (idle jiffies, total jiffies).
            per_core: vec![(100, 1000), (200, 1000), (300, 1000)],
        };
        // The third core has no elapsed jiffies, so its use must be 0.
        let cur = CpuSample {
            per_core: vec![(150, 1100), (200, 1100), (300, 1000)],
        };
        assert_eq!(diff_per_core(&prev, &cur), vec![50, 100, 0]);
    }

    /// Test that the collector reads real values from `/proc`. The first poll
    /// has no previous sample, so its CPU use must be zero.
    ///   1. Poll one time and check zero CPU use and valid memory values
    ///   2. Poll again and check that each core is at most 100 percent
    #[cfg(target_os = "linux")]
    #[test]
    fn collector_polls_proc_with_zero_cpu_first_then_bounded_usage() {
        let mut collector = Collector::new();
        let first = collector.poll();
        assert!(!first.per_core.is_empty());
        assert!(first.per_core.iter().all(|&v| v == 0));
        assert!(first.total_bytes > 0);
        assert!(first.used_bytes <= first.total_bytes);

        let second = collector.poll();
        assert_eq!(second.per_core.len(), first.per_core.len());
        assert!(second.per_core.iter().all(|&v| v <= 100));
    }
}
