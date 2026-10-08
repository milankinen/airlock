//! Optional raw copy of guest output.
//!
//! Writes a copy of the guest output to a file, so that a developer can
//! replay and examine the session later.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::OutputSink;
use crate::cli;

/// Output sink that forwards guest output to `inner`. When a dump file is
/// open, it also appends each stdout and stderr chunk to the file,
/// unchanged.
pub struct DumpSink<'a, S: OutputSink> {
    inner: &'a mut S,
    file: Option<File>,
}

impl<'a, S: OutputSink> DumpSink<'a, S> {
    /// Make a sink that copies the output into `file`. When `file` is
    /// `None`, it only forwards the output.
    pub fn new(inner: &'a mut S, file: Option<File>) -> Self {
        Self { inner, file }
    }

    /// Make a sink that copies the output into `<sandbox_dir>/pty.dump`
    /// when `AIRLOCK_PTY_DUMP=1`. Truncates the file. If the file does not
    /// open, reports the error and makes no copy.
    pub fn from_env(inner: &'a mut S, sandbox_dir: &Path) -> Self {
        let file = if std::env::var("AIRLOCK_PTY_DUMP").as_deref() == Ok("1") {
            open_dump(&sandbox_dir.join("pty.dump"))
        } else {
            None
        };
        Self::new(inner, file)
    }

    fn dump(&mut self, bytes: &[u8]) {
        if let Some(f) = &mut self.file {
            let _ = f.write_all(bytes);
        }
    }
}

impl<S: OutputSink> OutputSink for DumpSink<'_, S> {
    fn stdout(&mut self, bytes: &[u8]) {
        self.dump(bytes);
        self.inner.stdout(bytes);
    }

    fn stderr(&mut self, bytes: &[u8]) {
        self.dump(bytes);
        self.inner.stderr(bytes);
    }
}

/// Create the dump file. Reports the result to the user.
fn open_dump(path: &Path) -> Option<File> {
    match File::create(path) {
        Ok(f) => {
            cli::log!("PTY dump: {}", path.display());
            Some(f)
        }
        Err(e) => {
            cli::error!("Failed to open PTY dump {}: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the raw copy of guest output to a dump file.

    use super::*;
    use crate::test_cfg::sinks::RecordingSink;
    use crate::test_cfg::temp_dir;

    /// Test that the dump sink forwards stdout and stderr to the inner sink
    /// and writes both to the dump file in the order they came.
    ///   1. Send stdout, stderr and stdout chunks through a dump sink
    ///   2. Check that the inner sink got each stream
    ///   3. Check that the dump file has all chunks in order
    #[test]
    fn dump_sink_forwards_both_streams_and_tees_them_in_order() {
        let dir = temp_dir();
        let path = dir.path().join("pty.dump");
        let mut rec = RecordingSink::default();
        {
            let mut sink = DumpSink::new(&mut rec, Some(File::create(&path).unwrap()));
            sink.stdout(b"out-1 ");
            sink.stderr(b"err ");
            sink.stdout(b"out-2");
        }
        assert_eq!(rec.out, b"out-1 out-2");
        assert_eq!(rec.err, b"err ");
        assert_eq!(std::fs::read(&path).unwrap(), b"out-1 err out-2");
    }
}
