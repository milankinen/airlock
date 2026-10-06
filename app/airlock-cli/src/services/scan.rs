//! The streaming scan of API answers: no real token of the provider
//! reaches the sandbox in an API answer, whatever its size or framing.
//!
//! [`scan_answer`] checks every answer of a service's API hosts:
//!
//! - A compressed answer is refused (local `502`): it cannot be scanned,
//!   and the proxy asked for none (`Accept-Encoding: identity`).
//! - A response header that holds a real token refuses the answer. That
//!   covers `Set-Cookie` too: a cookie that holds a JWT with an OpenAI
//!   claim makes the answer a `502`.
//! - The body and its trailers stream through a [`Scanner`]. The primary
//!   check is an exact search for the real values the proxy knows: every
//!   real token of the service's store and the real values of the masked
//!   secrets injected into this request (opaque tokens too). The second
//!   check searches for the provider's token shapes
//!   ([`super::tokens::Format::starts`] and [`super::tokens::Format::shape`]:
//!   `sk-ant-oat01-…` with 80 or more characters, JWTs with an OpenAI
//!   claim; surrogates excluded), at the start of a
//!   run of token characters only. An event stream (model output) gets the
//!   exact search only: model text can hold token-shaped strings.
//! - A JSON answer is held until [`HOLD_JSON`] bytes are scanned: a hit
//!   there is a local `502`. Other answers go on at once (the status
//!   included); a hit ends the stream with an error, so the chunk with the
//!   token never reaches the guest.
//! - A switch of protocols (`101`, the WebSocket upgrade) passes.
//!
//! Split tokens: a token is a run of token characters
//! ([`is_token_char`]). The scanner holds back the run at the end of a
//! chunk until it ends (at most [`HOLD_MAX`] bytes; a longer run goes on
//! in parts), and an end of the chunk that is the start of a known value.
//! A token
//! split across chunks is found whole. Everything before goes on at once:
//! an event stream, whose events end in a newline, is not delayed.
//!
//! Bounded work: each byte is checked once for the start of a candidate
//! token, a candidate is at most [`MAX_TOKEN_LEN`] bytes, and held bytes
//! are not searched again on the next chunk.
//!
//! Known limits:
//!
//! - WebSocket frames after a `101` are not scanned.
//! - An encoded form of a token (a JSON `\u` escape, base64,
//!   URL-encoding) is not found. The sandbox can make the proxy send a
//!   real token upstream only in the swapped credential headers, so an
//!   upstream that echoes a request cannot be steered to encode one.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Either};
use hyper::body::{Body, Frame};
use hyper::header::{CONTENT_ENCODING, CONTENT_TYPE};
use hyper::{HeaderMap, Response, StatusCode};

use super::oauth::server_error;
use super::store::Snapshot;
use super::tokens::Formats;
use crate::network::http::{BoxError, ResponseBody};
use crate::network::target::InjectedSecret;

/// The longest token the shape search decides on: longer runs are cut
/// there (a JWT of the providers is a few KiB).
pub const MAX_TOKEN_LEN: usize = 8 * 1024;

/// How much of a long run of token characters the scanner holds back
/// (at least [`MAX_TOKEN_LEN`]). A run up to twice this goes on whole.
pub const HOLD_MAX: usize = 32 * 1024;

/// How much of a JSON answer the scan reads before the answer goes on: a
/// JSON answer up to this size with a real token is a local `502`, not a
/// cut stream.
pub const HOLD_JSON: usize = 64 * 1024;

/// Store tokens shorter than this are not searched for: too likely to
/// match by chance.
const MIN_KNOWN_LEN: usize = 8;

/// Injected masked secrets shorter than this are not searched for: a user
/// secret can be short and common (masked secrets have 8 or more
/// characters), and a chance match would cut an answer.
pub const MIN_INJECTED_LEN: usize = 16;

/// A character of a token run: base64url, dots and `~`. The providers'
/// token shapes use no other; `=`, `/` and `+` end a run, so a token
/// after `name=` (a cookie) or in a URL path starts a run of its own.
pub fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~')
}

/// The real values an API answer must not carry: the real tokens of the
/// service's store and the real values of the masked secrets injected
/// into the request.
pub fn known_reals(snapshot: &Snapshot, injected: &[InjectedSecret]) -> Vec<String> {
    snapshot
        .reals()
        .filter(|r| r.len() >= MIN_KNOWN_LEN)
        .chain(
            injected
                .iter()
                .map(|s| s.real.as_str())
                .filter(|r| r.len() >= MIN_INJECTED_LEN),
        )
        .map(String::from)
        .collect()
}

/// A real token was found.
#[derive(Debug)]
pub struct Found;

impl std::fmt::Display for Found {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("airlock: the answer carries a real token; the proxy ended it")
    }
}

impl std::error::Error for Found {}

/// What the scanner looks for, and where it left off.
struct State {
    formats: &'static Formats,
    known: Vec<Vec<u8>>,
    /// Search the token shapes (not in event streams).
    shapes: bool,
    /// The byte before the held bytes (the last byte that went on).
    before: Option<u8>,
    /// Offset in the held bytes from which candidate starts are not yet
    /// decided.
    shapes_from: usize,
    /// The undecided candidate at `shapes_from`, and how far its run is
    /// known to reach.
    open_end: Option<usize>,
    /// Offset in the held bytes from which known values may still start.
    known_from: usize,
    /// The length of the run of token characters at the end of the held
    /// bytes.
    run: usize,
}

impl State {
    fn new(formats: &'static Formats, known: Vec<String>, shapes: bool) -> Self {
        Self {
            formats,
            known: known.into_iter().map(String::into_bytes).collect(),
            shapes,
            before: None,
            shapes_from: 0,
            open_end: None,
            known_from: 0,
            run: 0,
        }
    }

    fn longest_known(&self) -> usize {
        self.known.iter().map(Vec::len).max().unwrap_or(0)
    }

    /// The longest end of `buf` that is the start of a known value (and
    /// not the whole value): those bytes must wait for the next chunk.
    fn known_prefix_at_end(&self, buf: &[u8]) -> usize {
        let Some(&last) = buf.last() else {
            return 0;
        };
        self.known
            .iter()
            .flat_map(|k| {
                (1..k.len().min(buf.len() + 1))
                    .filter(move |&l| k[l - 1] == last && buf.ends_with(&k[..l]))
            })
            .max()
            .unwrap_or(0)
    }

    /// Search `buf` (the held bytes) from where the last search left off.
    /// `last`: no more data follows.
    fn check(&mut self, buf: &[u8], last: bool) -> Result<(), Found> {
        for real in &self.known {
            if find_from(buf, real, self.known_from) {
                return Err(Found);
            }
        }
        self.known_from = if last {
            buf.len()
        } else {
            buf.len()
                .saturating_sub(self.longest_known().saturating_sub(1))
                .max(self.known_from)
        };
        if !self.shapes {
            return Ok(());
        }
        let mut i = self.shapes_from;
        let mut undecided = None;
        while i < buf.len() {
            let prev = if i == 0 {
                self.before
            } else {
                Some(buf[i - 1])
            };
            if !is_token_char(buf[i]) || prev.is_some_and(is_token_char) {
                i += 1;
                continue;
            }
            // A run starts at `i`: the candidate, at most MAX_TOKEN_LEN.
            let cap = buf.len().min(i + MAX_TOKEN_LEN);
            let from = match self.open_end {
                Some(end) if i == self.shapes_from => end.min(cap),
                _ => i,
            };
            let end = (from..cap).find(|&j| !is_token_char(buf[j])).unwrap_or(cap);
            if !last && end == buf.len() && end - i < MAX_TOKEN_LEN {
                undecided = Some((i, end));
                break;
            }
            // Token characters are ASCII.
            let candidate = std::str::from_utf8(&buf[i..end]).unwrap_or_default();
            if self.is_real_shape(candidate) {
                return Err(Found);
            }
            i = end.max(i + 1);
        }
        (self.shapes_from, self.open_end) = match undecided {
            Some((start, end)) => (start, Some(end)),
            None => (buf.len(), None),
        };
        Ok(())
    }

    fn is_real_shape(&self, candidate: &str) -> bool {
        !self.formats.is_surrogate(candidate)
            && self
                .formats
                .0
                .iter()
                .any(|f| f.starts.iter().any(|p| candidate.starts_with(p)) && (f.shape)(candidate))
    }

    /// The held bytes lost their first `n` bytes (they went on).
    fn shift(&mut self, n: usize, last_sent: Option<u8>) {
        if n == 0 {
            return;
        }
        self.before = last_sent;
        self.shapes_from = self.shapes_from.saturating_sub(n);
        self.open_end = self.open_end.map(|e| e.saturating_sub(n));
        self.known_from = self.known_from.saturating_sub(n);
    }
}

/// The scan state of one body.
pub struct Scanner {
    state: State,
    /// Bytes held back: scanned, but they may be the start of a token.
    held: Vec<u8>,
}

impl Scanner {
    /// A scanner for the real values `known`; `shapes`: also search the
    /// provider's token shapes.
    pub fn new(formats: &'static Formats, known: Vec<String>, shapes: bool) -> Self {
        Self {
            state: State::new(formats, known, shapes),
            held: Vec::new(),
        }
    }

    /// Scan the next chunk. Returns the bytes that may go to the guest
    /// now (possibly none).
    pub fn push(&mut self, chunk: &[u8]) -> Result<Bytes, Found> {
        self.held.extend_from_slice(chunk);
        let tail = chunk
            .iter()
            .rev()
            .take_while(|c| is_token_char(**c))
            .count();
        self.state.run = if tail == chunk.len() {
            self.state.run + chunk.len()
        } else {
            tail
        };
        self.state.check(&self.held, false)?;
        let run = self.state.run.min(self.held.len());
        let run_hold = if run > 2 * HOLD_MAX { HOLD_MAX } else { run };
        let known_hold = self.state.known_prefix_at_end(&self.held);
        let emit = self.held.len() - run_hold.max(known_hold).min(self.held.len());
        let out: Vec<u8> = self.held.drain(..emit).collect();
        self.state.shift(emit, out.last().copied());
        Ok(Bytes::from(out))
    }

    /// The end of the body: the held bytes, scanned as complete.
    pub fn finish(&mut self) -> Result<Bytes, Found> {
        self.state.check(&self.held, true)?;
        Ok(Bytes::from(std::mem::take(&mut self.held)))
    }

    /// Whether a complete value (a header or trailer value) holds a real
    /// value or, whatever the content type, a token shape.
    pub fn holds_real(&self, value: &[u8]) -> bool {
        let known = self
            .state
            .known
            .iter()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect();
        State::new(self.state.formats, known, true)
            .check(value, true)
            .is_err()
    }

    fn holds_real_header(&self, headers: &HeaderMap) -> bool {
        headers.values().any(|v| self.holds_real(v.as_bytes()))
    }
}

/// Whether `needle` occurs in `haystack` at or after `from`.
fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> bool {
    let Some((&first, _)) = needle.split_first() else {
        return false;
    };
    (from..haystack.len())
        .filter(|&i| haystack[i] == first)
        .any(|i| haystack[i..].starts_with(needle))
}

/// The media type of the `Content-Type`, lowercase, without parameters.
fn mime_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| ct.split(';').next())
        .map(|mime| mime.trim().to_ascii_lowercase())
}

/// Check an answer of an API host (see the module docs). `known`: the
/// real values the answer must not carry ([`known_reals`]).
pub async fn scan_answer(
    resp: Response<ResponseBody>,
    formats: &'static Formats,
    known: Vec<String>,
) -> anyhow::Result<Response<ResponseBody>> {
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        return Ok(resp);
    }
    if resp
        .headers()
        .get_all(CONTENT_ENCODING)
        .iter()
        .any(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Ok(server_error("a compressed API answer"));
    }
    let mime = mime_type(resp.headers());
    let json = mime
        .as_deref()
        .is_some_and(|m| m == "application/json" || m.ends_with("+json"));
    let stream = mime.as_deref() == Some("text/event-stream");
    let mut scanner = Scanner::new(formats, known, !stream);
    if scanner.holds_real_header(resp.headers()) {
        return Ok(server_error("an API answer with a real token in a header"));
    }
    let hold = if json { HOLD_JSON } else { 0 };
    let (parts, mut body) = resp.into_parts();
    let mut ready: VecDeque<Frame<Bytes>> = VecDeque::new();
    let mut done = false;
    let mut held = 0;
    while held < hold && !done {
        let Some(frame) = body.frame().await else {
            match scanner.finish() {
                Ok(rest) if !rest.is_empty() => ready.push_back(Frame::data(rest)),
                Ok(_) => {}
                Err(Found) => return Ok(refused()),
            }
            done = true;
            break;
        };
        let frame = frame.map_err(|e| anyhow::anyhow!("API answer: {e}"))?;
        match frame.into_data() {
            Ok(data) => match scanner.push(&data) {
                Ok(out) => {
                    held += data.len();
                    if !out.is_empty() {
                        ready.push_back(Frame::data(out));
                    }
                }
                Err(Found) => return Ok(refused()),
            },
            Err(trailers) => {
                let rest = match scanner.finish() {
                    Ok(rest) => rest,
                    Err(Found) => return Ok(refused()),
                };
                if trailers
                    .trailers_ref()
                    .is_some_and(|t| scanner.holds_real_header(t))
                {
                    return Ok(refused());
                }
                if !rest.is_empty() {
                    ready.push_back(Frame::data(rest));
                }
                ready.push_back(trailers);
                done = true;
            }
        }
    }
    let scanned = ScanBody {
        inner: body,
        scanner,
        ready,
        done,
        failed: false,
    };
    Ok(Response::from_parts(
        parts,
        Either::Left(Either::Right(scanned.boxed_unsync())),
    ))
}

fn refused() -> Response<ResponseBody> {
    server_error("an API answer that carries a real token")
}

/// A body that passes through a [`Scanner`].
struct ScanBody {
    inner: ResponseBody,
    scanner: Scanner,
    /// Frames ready for the guest.
    ready: VecDeque<Frame<Bytes>>,
    /// The inner body ended (its rest is in `ready`).
    done: bool,
    /// A token was found: the stream ended with an error.
    failed: bool,
}

impl ScanBody {
    /// Scan one frame of the inner body; queue what may go on.
    fn scan(&mut self, frame: Option<Frame<Bytes>>) -> Result<(), Found> {
        let Some(frame) = frame else {
            self.done = true;
            let rest = self.scanner.finish()?;
            if !rest.is_empty() {
                self.ready.push_back(Frame::data(rest));
            }
            return Ok(());
        };
        match frame.into_data() {
            Ok(data) => {
                let out = self.scanner.push(&data)?;
                if !out.is_empty() {
                    self.ready.push_back(Frame::data(out));
                }
            }
            Err(trailers) => {
                self.done = true;
                let rest = self.scanner.finish()?;
                if trailers
                    .trailers_ref()
                    .is_some_and(|t| self.scanner.holds_real_header(t))
                {
                    return Err(Found);
                }
                if !rest.is_empty() {
                    self.ready.push_back(Frame::data(rest));
                }
                self.ready.push_back(trailers);
            }
        }
        Ok(())
    }
}

impl Body for ScanBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        loop {
            if let Some(frame) = this.ready.pop_front() {
                return Poll::Ready(Some(Ok(frame)));
            }
            if this.done || this.failed {
                return Poll::Ready(None);
            }
            let frame = match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => None,
                Poll::Ready(Some(Ok(frame))) => Some(frame),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
            };
            if let Err(found) = this.scan(frame) {
                tracing::warn!("ended an API answer that carries a real token");
                this.failed = true;
                this.ready.clear();
                return Poll::Ready(Some(Err(Box::new(found))));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{anthropic, openai};

    /// A real Anthropic token of a realistic shape.
    fn real(prefix: &str) -> String {
        format!("{prefix}-{}", "R".repeat(90))
    }

    fn scanner(formats: &'static Formats, known: &[&str], shapes: bool) -> Scanner {
        Scanner::new(
            formats,
            known.iter().map(ToString::to_string).collect(),
            shapes,
        )
    }

    /// Push `chunks`, then finish; the bytes that went out, or `None` on a
    /// hit.
    fn run(scanner: &mut Scanner, chunks: &[&str]) -> Option<String> {
        let mut out = Vec::new();
        for c in chunks {
            out.extend_from_slice(&scanner.push(c.as_bytes()).ok()?);
        }
        out.extend_from_slice(&scanner.finish().ok()?);
        Some(String::from_utf8(out).unwrap())
    }

    #[test]
    fn real_shapes_are_found_and_others_pass() {
        let a = &anthropic::FORMATS;
        for text in [
            format!(r#"{{"k":"{}"}}"#, real("sk-ant-oat01")),
            format!("Bearer {}", real("sk-ant-ort01")),
            real("sk-ant-api03"),
        ] {
            assert!(
                run(&mut scanner(a, &[], true), &[&text]).is_none(),
                "{text}"
            );
        }
        for text in [
            format!(r#"{{"k":"sk-ant-oat01-airlock-{}"}}"#, "s".repeat(64)),
            "sk-ant-api03-short example in a text".to_string(),
            format!("x{}", real("sk-ant-api03")),
            format!("assert_{}", real("sk-ant-api03")),
            format!("sk-ant-oat1-{}", "R".repeat(90)),
            "plain text without tokens\n".to_string(),
        ] {
            assert_eq!(
                run(&mut scanner(a, &[], true), &[&text]).as_deref(),
                Some(text.as_str()),
                "{text}"
            );
        }
    }

    /// An event stream gets the exact search only: a token shape in model
    /// output passes, a known real value does not.
    #[test]
    fn event_streams_get_the_exact_search_only() {
        let a = &anthropic::FORMATS;
        let shaped = format!("data: {}\n\n", real("sk-ant-api03"));
        assert_eq!(
            run(&mut scanner(a, &[], false), &[&shaped]).as_deref(),
            Some(shaped.as_str())
        );
        let known = "known-real-value-123";
        assert!(
            run(
                &mut scanner(a, &[known], false),
                &["data: known-real-", "value-123\n\n"]
            )
            .is_none()
        );
    }

    /// A token split across chunks is found; the bytes before it went
    /// out, the token's bytes did not.
    #[test]
    fn a_split_token_is_found() {
        let token = real("sk-ant-oat01");
        let (x, y) = token.split_at(20);
        let mut s = scanner(&anthropic::FORMATS, &[], true);
        assert_eq!(
            &s.push(format!("data: one\n\ndata: {x}").as_bytes())
                .unwrap()[..],
            b"data: one\n\ndata: "
        );
        assert!(s.push(y.as_bytes()).unwrap().is_empty());
        assert!(s.push(b"\n\n").is_err());
        // A surrogate split the same way passes whole.
        let surrogate = format!("sk-ant-oat01-airlock-{}", "s".repeat(64));
        let (x, y) = surrogate.split_at(15);
        let mut s = scanner(&anthropic::FORMATS, &[], true);
        assert_eq!(
            run(&mut s, &[x, y, "\n"]).unwrap(),
            format!("{surrogate}\n")
        );
    }

    /// Known values are found split, also with characters that end a run.
    #[test]
    fn known_values_are_found_split() {
        for known in ["REAL-opaque-access-token-1234", "pass word!secret"] {
            let (x, y) = known.split_at(7);
            let mut s = scanner(&openai::FORMATS, &[known], true);
            assert!(
                run(&mut s, &["{\"t\":\"", x, y, "\"}"]).is_none(),
                "{known}"
            );
        }
        let mut s = scanner(&openai::FORMATS, &["REAL-opaque-access-token-1234"], true);
        assert!(run(&mut s, &["{\"t\":\"REAL-opaque-other\"}"]).is_some());
    }

    #[test]
    fn openai_jwts_are_found() {
        use base64::Engine as _;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let jwt = |claims: &str| {
            format!(
                "eyJhbGciOiJSUzI1NiJ9.{}.sig",
                URL_SAFE_NO_PAD.encode(claims)
            )
        };
        let real = jwt(r#"{"iss":"https://auth.openai.com"}"#);
        let fake = crate::services::tokens::fake_jwt(&real).unwrap().unwrap();
        let other = jwt(r#"{"iss":"https://example.com"}"#);
        let o = &openai::FORMATS;
        let (x, y) = real.split_at(30);
        assert!(run(&mut scanner(o, &[], true), &["a ", x, y, " b"]).is_none());
        assert!(run(&mut scanner(o, &[], true), &[&format!("{real}.more")]).is_none());
        // `rt_…` is no shape the scan knows: the exact search finds stored
        // refresh tokens of any format.
        let short = format!("rt_{}", "a".repeat(40));
        let id = format!("assert_rt_refresh_token_{}", "a".repeat(40));
        for ok in [fake.as_str(), other.as_str(), &short, &id] {
            assert_eq!(
                run(&mut scanner(o, &[], true), &[ok]).as_deref(),
                Some(ok),
                "{ok}"
            );
        }
    }

    /// A long run goes on in parts; a token right after the held part is
    /// still found.
    #[test]
    fn a_long_run_is_not_held_whole() {
        let mut s = scanner(&anthropic::FORMATS, &[], true);
        let long = "a".repeat(2 * HOLD_MAX + 100);
        assert_eq!(s.push(long.as_bytes()).unwrap().len(), HOLD_MAX + 100);
        let mut s = scanner(&anthropic::FORMATS, &[], true);
        assert!(
            s.push(format!("x {}\n", real("sk-ant-oat01")).as_bytes())
                .is_err()
        );
    }

    /// Adversarial runs cost bounded work: 1 MB of candidate starts, in
    /// small and large chunks, each in well under a second (debug build).
    #[test]
    fn adversarial_runs_are_scanned_fast() {
        for (pattern, chunk) in [
            ("-eyJ", 16),
            ("-eyJ", 400 * 1024),
            (" eyJ", 16),
            (" eyJ", 400 * 1024),
            ("\nsk-ant-oat01-", 7),
            ("a", 1),
        ] {
            let started = std::time::Instant::now();
            let body = pattern.repeat(1024 * 1024 / pattern.len());
            let size = if pattern == "a" {
                64 * 1024
            } else {
                body.len()
            };
            let mut s = scanner(&openai::FORMATS, &["known-value-xyz"], true);
            let mut out = 0;
            for part in body.as_bytes()[..size].chunks(chunk) {
                out += s.push(part).unwrap().len();
            }
            out += s.finish().unwrap().len();
            assert_eq!(out, size, "{pattern:?}");
            let elapsed = started.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(1),
                "{pattern:?} {chunk}: {elapsed:?}"
            );
        }
    }

    #[test]
    fn holds_real_checks_header_values() {
        let s = scanner(&anthropic::FORMATS, &["known-real-value"], false);
        assert!(s.holds_real(format!("x {}", real("sk-ant-api03")).as_bytes()));
        assert!(s.holds_real(b"a=known-real-value; Path=/"));
        assert!(!s.holds_real(b"sk-ant-api03-airlock-abcdefghijklmnopqrstu"));
        assert!(!s.holds_real(b"text/event-stream"));
    }
}
