use crate::runtime::OutputSink;

/// An output sink that keeps everything it gets.
#[derive(Default)]
pub struct RecordingSink {
    pub out: Vec<u8>,
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
