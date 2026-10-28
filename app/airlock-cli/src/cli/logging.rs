//! Host log files.
//!
//! Writes the diagnostic logs of airlock to a log file, and not to the
//! terminal. The sandbox log goes to the sandbox directory. The logs from
//! before the sandbox is known stay in memory, and go to the file when the
//! sandbox directory is known. Commands that know their log file at the
//! start can write to it at once. The log files do not grow without limit.
//! A log file is cut to a limited tail when it opens, and again when the
//! process wrote much data to it.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{Arc, Mutex, Once};

use tracing_subscriber::EnvFilter;

use super::LogLevel;
use crate::util::verified_path::VerifiedPath;

/// Name of the log file in the sandbox directory.
pub const LOG_FILE: &str = "airlock.log";

/// Size of the tail that stays in a log file after a cut.
///
/// The file is cut when it opens, and when the process wrote more than
/// twice this size since the last cut. The start of the file is removed. Each
/// run thus appends to a limited tail, and the file is not deleted. This keeps
/// crash logs from the previous run available. One process keeps the file
/// below 3 times this size plus one log line.
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Maximum size of the logs in memory before the file is known. Later
/// lines are dropped.
const BUFFER_MAX_BYTES: usize = 1024 * 1024;

/// Guard for [`init`] and [`init_file`]. The subscriber can be set only
/// once per process.
static INIT: Once = Once::new();

/// Where the log lines go now.
static SINK: Mutex<Sink> = Mutex::new(Sink::Buffer(Vec::new()));

/// Log file with a size limit. All writes append to the end of the file.
/// Write errors are ignored, because a failed log write must not fail the
/// run.
struct LogFile {
    file: File,
    /// Text at the start of each write.
    prefix: Vec<u8>,
    /// Bytes that this process wrote since the last cut. The lock also
    /// keeps the writes and cuts of the threads in order.
    written: Mutex<u64>,
}

impl LogFile {
    /// Open the log file `name` in `dir`, and keep only the last
    /// [`LOG_MAX_BYTES`] of it.
    /// Args:
    ///  - `dir`: Directory of the log file
    ///  - `name`: File name of the log file
    ///  - `prefix`: Text at the start of each write, for example a process
    ///    id. Can be empty.
    ///
    /// Returns:
    ///   The log file, or error if it does not open. The file is never
    ///   opened through a symlink.
    fn open(dir: &VerifiedPath, name: &str, prefix: &str) -> io::Result<Self> {
        let file = dir.join(name)?.open_with(
            OpenOptions::new()
                .read(true)
                .append(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW),
        )?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other(format!("{name} is not a file")));
        }
        trim(&file);
        Ok(Self {
            file,
            prefix: prefix.as_bytes().to_vec(),
            written: Mutex::new(0),
        })
    }

    /// Append `buf` to the file, after the prefix. Ignores errors. When the
    /// process wrote more than twice [`LOG_MAX_BYTES`] since the last cut, cut
    /// the file again.
    fn write(&self, buf: &[u8]) {
        // One write for each log event. With append mode, the lines of
        // processes that use the same file then do not mix.
        let mut line = Vec::with_capacity(self.prefix.len() + buf.len());
        line.extend_from_slice(&self.prefix);
        line.extend_from_slice(buf);
        // A write of an other thread during a cut can be lost. Thus all
        // writes and cuts of this process hold the lock.
        let mut written = self
            .written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if (&self.file).write_all(&line).is_err() {
            return;
        }
        *written += line.len() as u64;
        if *written > 2 * LOG_MAX_BYTES {
            *written = 0;
            trim(&self.file);
        }
    }
}

impl Write for &LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        LogFile::write(self, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Destination of the log lines.
enum Sink {
    /// The log file is not known yet. Keep the lines in memory.
    Buffer(Vec<u8>),
    /// The open log file.
    File(LogFile),
    /// The log file did not open. Drop the lines.
    Off,
}

/// Writer of the subscriber. Each write goes to the current [`Sink`].
struct SinkWriter;

impl Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut sink = SINK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &mut *sink {
            Sink::Buffer(lines) if lines.len() + buf.len() <= BUFFER_MAX_BYTES => {
                lines.extend_from_slice(buf);
            }
            Sink::File(file) => file.write(buf),
            Sink::Buffer(_) | Sink::Off => {}
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
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

/// Send the `tracing` output to the log file `name` in `dir` at once.
/// Args:
///  - `dir`: Directory of the log file
///  - `name`: File name of the log file
///  - `log_level`: Log level for the file
///  - `prefix`: Text at the start of each line
///
/// Only the first call in a process has an effect, also after [`init`].
/// If the file does not open, logging stays off and nothing fails.
pub fn init_file(dir: &VerifiedPath, name: &str, log_level: LogLevel, prefix: &str) {
    INIT.call_once(|| {
        let Ok(file) = LogFile::open(dir, name, prefix) else {
            return;
        };
        let _ = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(log_level.filter()))
            .with_writer(Arc::new(file))
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
pub fn attach(sandbox_dir: &VerifiedPath) {
    attach_with(&SINK, sandbox_dir);
}

/// Do [`attach`] with the destination `sink`. Tests use their own
/// destination, so that the order of the tests in the process has no
/// effect.
fn attach_with(sink: &Mutex<Sink>, sandbox_dir: &VerifiedPath) {
    let mut sink = sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Sink::Buffer(lines) = &mut *sink else {
        return;
    };
    let lines = std::mem::take(lines);
    *sink = match LogFile::open(sandbox_dir, LOG_FILE, "") {
        Ok(file) => {
            file.write(&lines);
            Sink::File(file)
        }
        Err(_) => Sink::Off,
    };
}

/// Keep only the last [`LOG_MAX_BYTES`] of the log file `file`.
///
/// The file then has [`LOG_MAX_BYTES`] of history or less. Errors are ignored.
/// Logging still works, but the file stays large.
// The threads of one process do not write during a cut. Other processes
// can append to the same file at the same time. Their lines between the
// read and the cut are lost, and two processes that cut at the same time
// can write the tail two times. The limit is thus best effort. A lock
// between processes is not worth it for a log file.
fn trim(mut file: &File) {
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
    // In append mode, the write goes to the end, which is now the start.
    let _ = file.set_len(0);
    let _ = file.write_all(&tail);
}

#[cfg(test)]
mod tests {
    //! Tests for the log file setup.

    use super::*;
    use crate::test_cfg::oci::verified_dir;
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
        attach_with(&sink, &verified_dir(first.path()));
        attach_with(&sink, &verified_dir(second.path()));

        let after = std::fs::read(&path).unwrap();
        let limit = usize::try_from(LOG_MAX_BYTES).unwrap();
        assert!(after.len() > limit);
        assert!(after[..limit].iter().all(|&b| b == b'b'));
        assert!(String::from_utf8_lossy(&after[limit..]).contains("early-line"));
        assert!(!second.path().join(LOG_FILE).exists());
    }

    /// Test that a process that writes much data keeps its log file small,
    /// and that the newest line stays.
    ///   1. Open the log file
    ///   2. Write lines for more than 3 times the size limit
    ///   3. Check that the file is not larger than 3 times the limit
    ///   4. Check that the last line is at the end of the file
    #[test]
    fn log_file_cuts_itself_when_process_writes_much_data() {
        let dir = temp_dir();
        let log = LogFile::open(&verified_dir(dir.path()), "test.log", "").unwrap();
        let path = dir.path().join("test.log");
        let line = format!("{}\n", "x".repeat(99));
        let count = 3 * usize::try_from(LOG_MAX_BYTES).unwrap() / line.len() + 1;
        let mut max_len = 0;
        for _ in 0..count {
            log.write(line.as_bytes());
            max_len = max_len.max(std::fs::metadata(&path).unwrap().len());
        }
        log.write(b"last line\n");

        // The cut runs right after the write that passes the limit, so the
        // file never exceeds the limit by more than the new line.
        assert!(max_len <= 3 * LOG_MAX_BYTES + line.len() as u64);
        assert!(std::fs::metadata(&path).unwrap().len() <= 3 * LOG_MAX_BYTES);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .ends_with("x\nlast line\n")
        );
    }

    /// Test that two writers of one log file append their lines with their
    /// prefix, and that the log file does not open through a symlink.
    /// Several ACP proxies write to one log file at the same time.
    ///   1. Open the log file two times with different prefixes
    ///   2. Write a line with each and check that the file has both lines
    ///   3. Put a symlink at an other log name and check that the open fails
    ///      and the link target stays empty
    #[test]
    fn log_file_appends_prefixed_lines_and_refuses_symlink() {
        let dir = temp_dir();
        let verified = verified_dir(dir.path());
        let first = LogFile::open(&verified, "test.log", "1 ").unwrap();
        let second = LogFile::open(&verified, "test.log", "2 ").unwrap();
        first.write(b"one\n");
        second.write(b"two\n");
        first.write(b"three\n");
        let text = std::fs::read_to_string(dir.path().join("test.log")).unwrap();
        assert_eq!(text, "1 one\n2 two\n1 three\n");

        let target = dir.path().join("target");
        std::fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("link.log")).unwrap();
        assert!(LogFile::open(&verified, "link.log", "").is_err());
        assert!(std::fs::read(&target).unwrap().is_empty());
    }
}
