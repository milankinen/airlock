//! Host log file.
//!
//! Writes the diagnostic logs of airlock to a log file in the project, and not
//! to the terminal.

use std::path::Path;
use std::sync::Once;

use tracing_subscriber::EnvFilter;

use super::LogLevel;

/// Maximum size of `airlock.log` at startup.
///
/// If the file is larger, the start of the file is removed. Each run thus
/// appends to a limited tail, and the file is not deleted. This keeps crash
/// logs from the previous run available.
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Guard for [`init`]. The subscriber can be set only once per process.
static INIT: Once = Once::new();

/// Send `tracing` output to `<cache_dir>/airlock.log`.
/// Args:
///  - `log_level`: Log level for the file
///  - `cache_dir`: Project `.airlock/` directory
///
/// Only the first call in a process has an effect. Later calls do nothing, so
/// the first level and file stay in use. A command that boots many sandboxes
/// in one run can call it again. If the file does not open, or a subscriber
/// is already set, logging stays off and nothing fails.
// The file is first made smaller if necessary (see [`LOG_MAX_BYTES`]).
pub fn init(log_level: LogLevel, cache_dir: &Path) {
    init_with(&INIT, log_level, cache_dir);
}

/// Do [`init`] with the guard `once`. Tests use their own guard, so that
/// the order of the tests in the process has no effect.
fn init_with(once: &Once, log_level: LogLevel, cache_dir: &Path) {
    once.call_once(|| {
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

/// Keep only the last [`LOG_MAX_BYTES`] of the log file at `path`.
///
/// The new run thus starts with 1 MB of history or less. Errors are ignored.
/// Logging still works, but the file stays large.
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
    //! Tests for the log file setup.

    use super::*;
    use crate::test_cfg::temp_dir;

    /// Test that the log setup cuts an oversized log file to its last part, and
    /// that only the first setup call has an effect.
    ///   1. Write a log that is 10 bytes longer than the limit
    ///   2. Set up the log in one directory, then in a second directory
    ///   3. Check that the log starts with the last bytes of the old content
    ///   4. Check that the second directory has no log file
    #[test]
    fn init_trims_oversized_log_to_its_tail_and_later_calls_do_nothing() {
        let first = temp_dir();
        let second = temp_dir();
        let path = first.path().join("airlock.log");
        let mut content = vec![b'a'; 10];
        content.extend(vec![b'b'; usize::try_from(LOG_MAX_BYTES).unwrap()]);
        std::fs::write(&path, &content).unwrap();

        // A guard of the test, not the guard of the process. Thus another
        // test that sets up the log first has no effect here.
        let once = Once::new();
        init_with(&once, LogLevel::Info, first.path());
        init_with(&once, LogLevel::Debug, second.path());

        let after = std::fs::read(&path).unwrap();
        // The setup can add new log lines after the kept part.
        assert!(after.len() as u64 >= LOG_MAX_BYTES);
        assert!(
            after[..usize::try_from(LOG_MAX_BYTES).unwrap()]
                .iter()
                .all(|&b| b == b'b')
        );
        assert!(!second.path().join("airlock.log").exists());
    }
}
