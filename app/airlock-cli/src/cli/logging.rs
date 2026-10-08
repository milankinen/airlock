//! Host log file.
//!
//! Writes the diagnostic logs of airlock to a log file in the sandbox
//! directory, and not to the terminal. The logs from before the sandbox is
//! known stay in memory, and go to the file when the sandbox directory is
//! known. The log file does not grow without limit.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Mutex, Once};

use tracing_subscriber::EnvFilter;

use super::LogLevel;
use crate::util::PinnedDir;

/// Name of the log file in the sandbox directory.
pub const LOG_FILE: &str = "airlock.log";

/// Maximum size of `airlock.log` at startup.
///
/// If the file is larger, the start of the file is removed. Each run thus
/// appends to a limited tail, and the file is not deleted. This keeps crash
/// logs from the previous run available.
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Maximum size of the logs in memory before the file is known. Later
/// lines are dropped.
const BUFFER_MAX_BYTES: usize = 1024 * 1024;

/// Guard for [`init`]. The subscriber can be set only once per process.
static INIT: Once = Once::new();

/// Where the log lines go now.
static SINK: Mutex<Sink> = Mutex::new(Sink::Buffer(Vec::new()));

/// Destination of the log lines.
enum Sink {
    /// The log file is not known yet. Keep the lines in memory.
    Buffer(Vec<u8>),
    /// The open log file.
    File(File),
    /// The log file did not open. Drop the lines.
    Off,
}

/// Writer of the subscriber. Each write goes to the current [`Sink`].
struct SinkWriter;

impl Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut sink = SINK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &mut *sink {
            Sink::Buffer(lines) if lines.len() + buf.len() <= BUFFER_MAX_BYTES => {
                lines.extend_from_slice(buf);
            }
            // Logging is best effort. A failed write must not fail the run.
            Sink::File(file) => {
                let _ = file.write_all(buf);
            }
            Sink::Buffer(_) | Sink::Off => {}
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Start to collect `tracing` output for the log file. The lines stay in
/// memory until [`attach`] gives the sandbox directory.
/// Args:
///  - `log_level`: Log level for the file
///
/// Only the first call in a process has an effect. Later calls do nothing, so
/// the first level stays in use. If a subscriber is already set, logging
/// stays off and nothing fails.
pub fn init(log_level: LogLevel) {
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(log_level.filter()))
            .with_writer(|| SinkWriter)
            .with_ansi(false)
            .try_init();
    });
}

/// Send the log to `<sandbox_dir>/airlock.log`, after the lines that are in
/// memory.
///
/// Only the first call in a process has an effect. Later calls do nothing, so
/// the first file stays in use. A command that boots many sandboxes in one
/// run can call it again. If the file does not open, logging stays off and
/// nothing fails. The file is never opened through a symlink.
// The file is first made smaller if necessary (see [`LOG_MAX_BYTES`]).
pub fn attach(sandbox_dir: &Path) {
    attach_with(&SINK, sandbox_dir);
}

/// Do [`attach`] with the destination `sink`. Tests use their own
/// destination, so that the order of the tests in the process has no
/// effect.
fn attach_with(sink: &Mutex<Sink>, sandbox_dir: &Path) {
    let mut sink = sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Sink::Buffer(lines) = &mut *sink else {
        return;
    };
    let lines = std::mem::take(lines);
    let opened = PinnedDir::pin(sandbox_dir).and_then(|dir| dir.open_read_write(LOG_FILE, 0o600));
    *sink = match opened {
        Ok(mut file) => {
            rotate_log(&mut file);
            let _ = file
                .seek(SeekFrom::End(0))
                .and_then(|_| file.write_all(&lines));
            Sink::File(file)
        }
        Err(_) => Sink::Off,
    };
}

/// Keep only the last [`LOG_MAX_BYTES`] of the log file `file`.
///
/// The new run thus starts with 1 MB of history or less. Errors are ignored.
/// Logging still works, but the file stays large.
fn rotate_log(file: &mut File) {
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return;
    };
    if len <= LOG_MAX_BYTES {
        return;
    }
    if file.seek(SeekFrom::Start(len - LOG_MAX_BYTES)).is_err() {
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

    /// Test that the log setup cuts an oversized log file to its last part,
    /// writes the lines from before the setup, and that only the first file
    /// is used.
    ///   1. Write a log that is 10 bytes longer than the limit
    ///   2. Keep a line in memory, then give the first and the second
    ///      directory
    ///   3. Check that the log starts with the last bytes of the old content
    ///      and has the line
    ///   4. Check that the second directory has no log file
    #[test]
    fn attach_trims_oversized_log_keeps_early_lines_and_later_calls_do_nothing() {
        let first = temp_dir();
        let second = temp_dir();
        let path = first.path().join(LOG_FILE);
        let mut content = vec![b'a'; 10];
        content.extend(vec![b'b'; usize::try_from(LOG_MAX_BYTES).unwrap()]);
        std::fs::write(&path, &content).unwrap();

        // A destination of the test, not the destination of the process.
        // Thus another test that sets up the log first has no effect here.
        let sink = Mutex::new(Sink::Buffer(b"early-line\n".to_vec()));
        attach_with(&sink, first.path());
        attach_with(&sink, second.path());

        let after = std::fs::read(&path).unwrap();
        let limit = usize::try_from(LOG_MAX_BYTES).unwrap();
        assert!(after.len() > limit);
        assert!(after[..limit].iter().all(|&b| b == b'b'));
        assert!(String::from_utf8_lossy(&after[limit..]).contains("early-line"));
        assert!(!second.path().join(LOG_FILE).exists());
    }
}
