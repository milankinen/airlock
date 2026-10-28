//! Server-sent events.
//!
//! Reads an event stream in parts of any size and gives the complete
//! events. Accepts all three kinds of line ends, also when a part ends
//! between the two characters of a line end. Keeps the last event ID and
//! the reconnect time that the server sends. A line or an event over the
//! size limit is an error, so that a bad server cannot make the host keep
//! data without limit.

use std::collections::VecDeque;
use std::time::Duration;

/// One complete event.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Event {
    /// Event type. Empty if the server did not set one.
    pub(super) kind: String,
    /// Data lines, joined with line feeds.
    pub(super) data: String,
}

/// Error when a line or an event is longer than the limit.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TooLong;

/// Incremental parser of one event stream.
pub(super) struct Parser {
    /// Largest line and largest event data in bytes.
    max: usize,
    /// Start of the current line, from earlier parts.
    line: Vec<u8>,
    /// Set after a carriage return. A line feed that comes next belongs to
    /// the same line end.
    after_cr: bool,
    /// Set after the first line. Only the first line can start with a byte
    /// order mark.
    first_line_done: bool,
    /// Event type of the event that is in progress.
    kind: String,
    /// Data of the event that is in progress, each line with a line feed.
    data: String,
    /// Set when the event in progress has a data line.
    has_data: bool,
    /// Event ID of the event that is in progress, or of the last event.
    id_buffer: Option<String>,
    /// Event ID of the last complete event, for a reconnect.
    last_id: Option<String>,
    /// Reconnect time from the server.
    retry: Option<Duration>,
    /// Complete events that the caller did not take yet.
    ready: VecDeque<Event>,
}

impl Parser {
    /// Make a parser with `max` as the limit of each line and each event
    /// in bytes.
    pub(super) fn new(max: usize) -> Self {
        Self {
            max,
            line: Vec::new(),
            after_cr: false,
            first_line_done: false,
            kind: String::new(),
            data: String::new(),
            has_data: false,
            id_buffer: None,
            last_id: None,
            retry: None,
            ready: VecDeque::new(),
        }
    }

    /// Read the next part of the stream.
    /// Returns:
    ///   Error if a line or an event is longer than the limit. The stream
    ///   is then not usable.
    pub(super) fn feed(&mut self, mut chunk: &[u8]) -> Result<(), TooLong> {
        // An empty part must keep the state of a carriage return.
        if chunk.is_empty() {
            return Ok(());
        }
        if self.after_cr && chunk.first() == Some(&b'\n') {
            chunk = &chunk[1..];
        }
        self.after_cr = false;
        while let Some(end) = chunk.iter().position(|&b| b == b'\n' || b == b'\r') {
            self.push_line_part(&chunk[..end])?;
            let line = std::mem::take(&mut self.line);
            self.line(&line)?;
            let mut next = end + 1;
            if chunk[end] == b'\r' {
                match chunk.get(next) {
                    Some(b'\n') => next += 1,
                    Some(_) => {}
                    // The line feed of a CRLF can come in the next part.
                    None => self.after_cr = true,
                }
            }
            chunk = &chunk[next..];
        }
        self.push_line_part(chunk)
    }

    /// Take the next complete event.
    pub(super) fn next_event(&mut self) -> Option<Event> {
        self.ready.pop_front()
    }

    /// Event ID of the last complete event, for a reconnect. `None` if
    /// the server set no ID or an empty ID.
    pub(super) fn last_id(&self) -> Option<&str> {
        self.last_id.as_deref().filter(|id| !id.is_empty())
    }

    /// Continue from an earlier stream: `id` is its last event ID.
    pub(super) fn set_last_id(&mut self, id: Option<String>) {
        self.id_buffer.clone_from(&id);
        self.last_id = id;
    }

    /// Reconnect time from the server.
    pub(super) fn retry(&self) -> Option<Duration> {
        self.retry
    }

    /// Add `part` to the current line.
    fn push_line_part(&mut self, part: &[u8]) -> Result<(), TooLong> {
        if self.line.len() + part.len() > self.max {
            return Err(TooLong);
        }
        self.line.extend_from_slice(part);
        Ok(())
    }

    /// Process one complete line, without its line end.
    fn line(&mut self, mut line: &[u8]) -> Result<(), TooLong> {
        if !self.first_line_done {
            self.first_line_done = true;
            line = line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line);
        }
        if line.is_empty() {
            self.dispatch();
            return Ok(());
        }
        if line[0] == b':' {
            return Ok(());
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(i) => {
                let value = &line[i + 1..];
                (&line[..i], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line, &b""[..]),
        };
        let value = String::from_utf8_lossy(value);
        match field {
            b"event" => self.kind = value.into_owned(),
            b"data" => {
                if self.data.len() + value.len() + 1 > self.max {
                    return Err(TooLong);
                }
                self.data.push_str(&value);
                self.data.push('\n');
                self.has_data = true;
            }
            // An ID with a NUL character is not valid. The spec says to
            // ignore it.
            b"id" if !value.contains('\0') => self.id_buffer = Some(value.into_owned()),
            b"retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                if let Ok(ms) = value.parse() {
                    self.retry = Some(Duration::from_millis(ms));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// End the event in progress at an empty line. An event without data
    /// is not given to the caller.
    fn dispatch(&mut self) {
        // The ID counts only for a complete event, also without data.
        // Thus a reconnect does not skip an event that broke.
        self.last_id.clone_from(&self.id_buffer);
        let kind = std::mem::take(&mut self.kind);
        let mut data = std::mem::take(&mut self.data);
        if !std::mem::take(&mut self.has_data) {
            return;
        }
        data.pop();
        self.ready.push_back(Event { kind, data });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that the parser gives the right events, last ID and reconnect
    /// time for streams in parts of any size, and refuses data over the
    /// limit.
    ///   1. Feed each stream of the table in the given parts
    ///   2. Check the events, the last ID, the reconnect time and the size
    ///      error
    #[test]
    fn parser_gives_events_for_each_stream_shape() {
        struct Case {
            name: &'static str,
            parts: &'static [&'static [u8]],
            events: &'static [(&'static str, &'static str)],
            last_id: Option<&'static str>,
            retry: Option<u64>,
            too_long: bool,
        }
        fn ok(
            name: &'static str,
            parts: &'static [&'static [u8]],
            events: &'static [(&'static str, &'static str)],
        ) -> Case {
            Case {
                name,
                parts,
                events,
                last_id: None,
                retry: None,
                too_long: false,
            }
        }
        let cases = [
            ok(
                "multi-line data",
                &[b"data: a\ndata:b\ndata\n\n"],
                &[("", "a\nb\n")],
            ),
            ok(
                "crlf split between parts",
                &[b"event: message\r", b"", b"\ndata: x\r", b"\n\r", b"\n"],
                &[("message", "x")],
            ),
            ok(
                "cr only line ends",
                &[b"data: 1\r\rdata: 2\r\r"],
                &[("", "1"), ("", "2")],
            ),
            ok(
                "comments and unknown fields",
                &[b": ping\nfoo: bar\ndata: y\n\n"],
                &[("", "y")],
            ),
            ok(
                "event without data is not given",
                &[b"event: x\n\ndata: z\n\n"],
                &[("", "z")],
            ),
            ok(
                "bom at start",
                &[b"\xEF\xBB", b"\xBFdata: b\n\n"],
                &[("", "b")],
            ),
            ok(
                "incomplete event is held",
                &[b"data: 1\n\ndata: 2\n"],
                &[("", "1")],
            ),
            Case {
                name: "id and retry",
                parts: &[b"id: 7\nretry: 1500\ndata: d\n\nid: a\0b\nretry: x\n\nid: 8\n"],
                events: &[("", "d")],
                last_id: Some("7"),
                retry: Some(1500),
                too_long: false,
            },
            ok(
                "empty id resets last id",
                &[b"id: 1\ndata: a\n\nid\ndata: b\n\n"],
                &[("", "a"), ("", "b")],
            ),
            Case {
                name: "line over limit",
                parts: &[b"data: 0123456789", b"0123456789"],
                too_long: true,
                ..ok("", &[], &[])
            },
            Case {
                name: "event over limit",
                parts: &[b"data: 0123456789\ndata: 0123456789\ndata: 0123456789\n"],
                too_long: true,
                ..ok("", &[], &[])
            },
        ];
        for case in cases {
            let mut parser = Parser::new(24);
            let fed: Result<(), TooLong> = case.parts.iter().try_for_each(|p| parser.feed(p));
            assert_eq!(fed.is_err(), case.too_long, "{}", case.name);
            let events: Vec<(String, String)> = std::iter::from_fn(|| parser.next_event())
                .map(|e| (e.kind, e.data))
                .collect();
            let expected: Vec<(String, String)> = case
                .events
                .iter()
                .map(|(k, d)| ((*k).into(), (*d).into()))
                .collect();
            assert_eq!(events, expected, "{}", case.name);
            assert_eq!(parser.last_id(), case.last_id, "{}", case.name);
            assert_eq!(
                parser.retry(),
                case.retry.map(Duration::from_millis),
                "{}",
                case.name
            );
        }
    }
}
