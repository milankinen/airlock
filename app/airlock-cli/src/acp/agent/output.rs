//! Output of one agent process.
//!
//! Splits the agent stdout into lines and gives the lines and the agent
//! exit in order. The exit is the last item. The memory use has a limit:
//! a line that is too long, also a line without its end, gives an error
//! item, and the stdout until the next newline is dropped. A line without
//! a newline at the exit is a line too. If the relay of the agent stops
//! without the exit, the agent exit code is 1.
//!
//! The agent stderr goes to the host stderr. The agent never waits for the
//! host stderr. When the host stderr is slow, the agent stderr is dropped
//! and counted.

use std::collections::VecDeque;

use futures::stream::{self, LocalBoxStream, StreamExt};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use super::Stderr;
use super::process::trace;
use crate::rpc::ProcessEvent;

/// One item of the agent output.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Output {
    /// One stdout line, without its newline.
    Line(Vec<u8>),
    /// The agent wrote a line that is longer than the limit. The consumer
    /// must kill the agent. More items can still come until the exit.
    TooLong,
    /// The agent exited with this exit code. This is the last item.
    Exit(i32),
}

/// Make the output stream of an agent process.
/// Args:
///  - `events`: Output and exit events of the agent process
///  - `max_line_bytes`: Maximum length of one stdout line
///  - `stderr`: Destination of the agent stderr
///  - `sandbox`: Sandbox of the agent, for the log
///
/// Returns:
///   The stream of the agent output. It ends after the exit.
pub(super) fn output(
    events: mpsc::Receiver<ProcessEvent>,
    max_line_bytes: usize,
    stderr: Stderr,
    sandbox: String,
) -> LocalBoxStream<'static, Output> {
    let framer = Framer {
        events: Some(events),
        max: max_line_bytes,
        stderr,
        sandbox,
        partial: Vec::new(),
        discard: false,
        ready: VecDeque::new(),
    };
    stream::unfold(framer, |mut framer| async move {
        loop {
            if let Some(item) = framer.ready.pop_front() {
                return Some((item, framer));
            }
            let events = framer.events.as_mut()?;
            // A relay that ends without the exit event lost the agent.
            let event = events.recv().await.unwrap_or(ProcessEvent::Exit(1));
            framer.on_event(event);
        }
    })
    // The unfold must not run again after its end.
    .fuse()
    .boxed_local()
}

/// Send the agent stderr `data` to the host stderr. Drop it if the stderr
/// queue is full.
/// Args:
///  - `stderr`: Destination of the agent stderr
///  - `sandbox`: Sandbox of the agent, for the log
///  - `data`: Agent stderr bytes
// The agent must not wait for the host stderr. An editor that does not
// read it would stop the relay.
pub(super) fn relay_stderr(stderr: &Stderr, sandbox: &str, data: Vec<u8>) {
    let len = data.len() as u64;
    if let Err(TrySendError::Full(_)) = stderr.tx.try_send(data) {
        if stderr.dropped.get() == 0 {
            tracing::warn!(sandbox, "drop agent stderr: stderr is full");
        }
        stderr.dropped.set(stderr.dropped.get() + len);
    }
}

/// State of the output stream.
struct Framer {
    /// Agent events, or `None` after the exit.
    events: Option<mpsc::Receiver<ProcessEvent>>,
    max: usize,
    stderr: Stderr,
    sandbox: String,
    /// Stdout bytes after the last complete line. They have no newline.
    partial: Vec<u8>,
    /// Drop the stdout until the next newline, after a too long line.
    discard: bool,
    /// Items for the stream, the oldest first.
    ready: VecDeque<Output>,
}

impl Framer {
    /// Handle one agent event.
    fn on_event(&mut self, event: ProcessEvent) {
        match event {
            ProcessEvent::Stdout(data) => self.on_stdout(&data),
            ProcessEvent::Stderr(data) => relay_stderr(&self.stderr, &self.sandbox, data),
            ProcessEvent::Exit(code) => {
                tracing::info!(sandbox = self.sandbox, code, "agent exited");
                if !self.partial.is_empty() {
                    let line = std::mem::take(&mut self.partial);
                    self.push_line(line);
                }
                self.ready.push_back(Output::Exit(code));
                self.events = None;
            }
        }
    }

    /// Split the agent stdout into lines.
    fn on_stdout(&mut self, mut data: &[u8]) {
        if self.discard {
            let Some(pos) = data.iter().position(|&b| b == b'\n') else {
                return;
            };
            data = &data[pos + 1..];
            self.discard = false;
        }
        let start = self.partial.len();
        self.partial.extend_from_slice(data);
        // The partial line has no newline. Thus only the new bytes need a
        // search, and the time stays linear for long lines.
        if let Some(pos) = data.iter().rposition(|&b| b == b'\n') {
            let rest = self.partial.split_off(start + pos + 1);
            let complete = std::mem::replace(&mut self.partial, rest);
            for line in complete[..complete.len() - 1].split(|&b| b == b'\n') {
                if line.len() > self.max {
                    self.ready.push_back(Output::TooLong);
                } else {
                    self.push_line(line.to_vec());
                }
            }
        }
        // A line without its end must not fill the memory.
        if self.partial.len() > self.max {
            self.ready.push_back(Output::TooLong);
            self.partial = Vec::new();
            self.discard = true;
        }
    }

    /// Add the complete line `line` to the stream.
    fn push_line(&mut self, line: Vec<u8>) {
        trace(&self.sandbox, "agent", &line);
        self.ready.push_back(Output::Line(line));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    /// Result of one output stream run.
    struct Run {
        items: Vec<Output>,
        stderr: mpsc::Receiver<Vec<u8>>,
        dropped: Rc<Cell<u64>>,
    }

    /// Run the output stream over the agent events `events`. The relay
    /// ends after the events.
    async fn run(events: Vec<ProcessEvent>, max: usize, stderr_cap: usize) -> Run {
        let (tx, rx) = mpsc::channel(events.len() + 1);
        for event in events {
            tx.try_send(event).unwrap();
        }
        drop(tx);
        let (stderr_tx, stderr) = mpsc::channel(stderr_cap);
        let dropped = Rc::new(Cell::new(0));
        let sink = Stderr {
            tx: stderr_tx,
            dropped: dropped.clone(),
        };
        let items = output(rx, max, sink, "sb".into()).collect().await;
        Run {
            items,
            stderr,
            dropped,
        }
    }

    /// Make a stdout event with the text `data`.
    fn out(data: &str) -> ProcessEvent {
        ProcessEvent::Stdout(data.as_bytes().to_vec())
    }

    /// Make a line item with the text `data`.
    fn line(data: &str) -> Output {
        Output::Line(data.as_bytes().to_vec())
    }

    /// Test that lines split across stdout chunks come out whole and in
    /// order, and that a line without a newline at the exit comes out
    /// before the exit.
    ///   1. Send three lines in chunks that cut the lines, the last one
    ///      without a newline, then the exit
    ///   2. Check the lines and the exit code
    #[tokio::test]
    async fn lines_split_across_chunks_come_out_whole() {
        let events = vec![
            out("{\"a\""),
            out(":1}\n{\"b\":2}\n{\"c"),
            out("\":3}"),
            ProcessEvent::Exit(3),
        ];
        let run = run(events, 64, 1).await;
        let expected = vec![
            line("{\"a\":1}"),
            line("{\"b\":2}"),
            line("{\"c\":3}"),
            Output::Exit(3),
        ];
        assert_eq!(run.items, expected);
    }

    /// Test that a complete line over the limit gives an error item, and
    /// that the next lines still come out.
    ///   1. Send a too long line and a short line in one chunk
    ///   2. Check that the long line gives the error and the short line
    ///      comes out
    #[tokio::test]
    async fn too_long_line_gives_error_and_keeps_next_lines() {
        let events = vec![out("abcdef\nok\n"), ProcessEvent::Exit(0)];
        let run = run(events, 4, 1).await;
        assert_eq!(
            run.items,
            vec![Output::TooLong, line("ok"), Output::Exit(0)]
        );
    }

    /// Test that a line without its end over the limit gives one error
    /// item, and that its later bytes are dropped until the next newline.
    /// The memory must not keep a line without limit.
    ///   1. Send a too long line in three chunks, then a short line
    ///   2. Check that there is one error and the short line comes out
    #[tokio::test]
    async fn too_long_partial_line_is_dropped_until_newline() {
        let events = vec![
            out("abcdef"),
            out("ghij"),
            out("kl\nok\n"),
            ProcessEvent::Exit(0),
        ];
        let run = run(events, 4, 1).await;
        assert_eq!(
            run.items,
            vec![Output::TooLong, line("ok"), Output::Exit(0)]
        );
    }

    /// Test that a too long line without its end at the exit gives no
    /// line item. Its bytes were dropped, and a part of a line is not a
    /// message.
    ///   1. Send a too long line without a newline, then the exit
    ///   2. Check that there is one error and then the exit
    #[tokio::test]
    async fn discard_at_exit_gives_no_line() {
        let events = vec![out("abcdef"), out("gh"), ProcessEvent::Exit(0)];
        let run = run(events, 4, 1).await;
        assert_eq!(run.items, vec![Output::TooLong, Output::Exit(0)]);
    }

    /// Test that a relay that ends without the exit gives exit code 1, and
    /// that the line without a newline still comes out.
    ///   1. Send a line without a newline and end the relay
    ///   2. Check the line and exit code 1
    #[tokio::test]
    async fn relay_end_without_exit_gives_exit_code_1() {
        let run = run(vec![out("last")], 64, 1).await;
        assert_eq!(run.items, vec![line("last"), Output::Exit(1)]);
    }

    /// Test that agent stderr goes to the host stderr, and that stderr over
    /// a full queue is dropped and counted. The agent must not wait for
    /// the host stderr.
    ///   1. Send two stderr chunks to a stderr queue with space for one
    ///   2. Check that the first chunk is in the queue and the bytes of
    ///      the second are counted
    #[tokio::test]
    async fn stderr_over_full_queue_is_dropped_and_counted() {
        let events = vec![
            ProcessEvent::Stderr(b"aa".to_vec()),
            ProcessEvent::Stderr(b"bbb".to_vec()),
            ProcessEvent::Exit(0),
        ];
        let mut run = run(events, 64, 1).await;
        assert_eq!(run.items, vec![Output::Exit(0)]);
        assert_eq!(run.stderr.try_recv().unwrap(), b"aa");
        assert_eq!(run.dropped.get(), 3);
    }
}
