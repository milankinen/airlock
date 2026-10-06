//! Host log file: `.airlock/airlock.log` under the project.
//!
//! [`init`] installs the global `tracing` subscriber once per process. A
//! command that boots several sandboxes in one run may call it again; later
//! calls are no-ops, so the first level and file stay in effect.

use std::path::Path;
use std::sync::Once;

use tracing_subscriber::EnvFilter;

use super::LogLevel;

/// Hard cap on `airlock.log` at startup. If the existing file is
/// larger than this, we trim the beginning so each run appends to a
/// bounded tail rather than nuking the file (crash logs from the
/// previous run survive long enough to be useful).
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Guards [`init`]: the subscriber can be set once per process.
static INIT: Once = Once::new();

/// Send `tracing` output at `log_level` to `<cache_dir>/airlock.log`,
/// trimming the file first (see [`LOG_MAX_BYTES`]). Only the first call in
/// a process has an effect. Best-effort: when the file cannot be opened,
/// or a subscriber is already set, logging stays off and nothing fails.
pub fn init(log_level: LogLevel, cache_dir: &Path) {
    INIT.call_once(|| {
        let log_path = cache_dir.join("airlock.log");
        rotate_log(&log_path);
        if let Ok(log_file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::new(log_level.filter()))
                .with_writer(std::sync::Mutex::new(log_file))
                .with_ansi(false)
                .try_init();
        }
    });
}

/// If `airlock.log` exceeds [`LOG_MAX_BYTES`], rewrite the file with
/// just its last N bytes so the new run starts with ≤1 MB of history.
/// Best-effort; failure is silent (logging still works, just wasn't
/// trimmed).
fn rotate_log(path: &Path) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= LOG_MAX_BYTES {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return;
    };
    let skip = meta.len() - LOG_MAX_BYTES;
    if file.seek(SeekFrom::Start(skip)).is_err() {
        return;
    }
    let mut tail = Vec::with_capacity(LOG_MAX_BYTES as usize);
    if file.read_to_end(&mut tail).is_err() {
        return;
    }
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.write_all(&tail);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    /// A second `init` (a second boot in the same process) must not panic,
    /// and must not create a second log file.
    #[test]
    fn init_twice_is_a_no_op() {
        let first = TempDir::new("logging-first");
        let second = TempDir::new("logging-second");
        init(LogLevel::Info, first.path());
        init(LogLevel::Debug, second.path());
        assert!(!second.path().join("airlock.log").exists());
    }

    #[test]
    fn rotate_keeps_the_tail() {
        let dir = TempDir::new("logging-rotate");
        let path = dir.path().join("airlock.log");
        let mut content = vec![b'a'; 10];
        content.extend(vec![b'b'; LOG_MAX_BYTES as usize]);
        std::fs::write(&path, &content).unwrap();
        rotate_log(&path);
        let after = std::fs::read(&path).unwrap();
        assert_eq!(after.len() as u64, LOG_MAX_BYTES);
        assert!(after.iter().all(|&b| b == b'b'));
    }
}
