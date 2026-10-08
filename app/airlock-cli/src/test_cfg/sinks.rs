//! An output sink that records guest output for tests.

use crate::runtime::OutputSink;

/// An output sink that keeps all stdout and stderr data that it gets.
#[derive(Default)]
pub struct RecordingSink {
    /// All stdout bytes, in order.
    pub out: Vec<u8>,
    /// All stderr bytes, in order.
    pub err: Vec<u8>,
}

impl OutputSink for RecordingSink {
    fn stdout(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    fn stderr(&mut self, bytes: &[u8]) {
        self.err.extend_from_slice(bytes);
    }
}
