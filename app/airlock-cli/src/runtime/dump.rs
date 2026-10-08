//! Optional raw copy of guest output for offline replay and diagnosis.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::OutputSink;
use crate::cli;

/// Forwards guest output to `inner` and, when a dump file is open, also
/// appends every stdout and stderr chunk to it, unchanged.
pub struct DumpSink<'a, S: OutputSink> {
    inner: &'a mut S,
    file: Option<File>,
}

impl<'a, S: OutputSink> DumpSink<'a, S> {
    /// Tee into `file`, or only forward when it is `None`.
    pub fn new(inner: &'a mut S, file: Option<File>) -> Self {
        Self { inner, file }
    }

    /// Tee into `<sandbox_dir>/pty.dump` when `AIRLOCK_PTY_DUMP=1`. The file
    /// is truncated; failing to open it is reported and dumping is skipped.
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
    use super::*;
    use crate::test_cfg::sinks::RecordingSink;
    use crate::test_cfg::temp_dir;

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
