//! The scan of API answers.
//!
//! Makes sure that no real token of the provider gets to the sandbox in an
//! API answer. The scan works on the stream, for answers of all sizes and
//! framings.
//!
//! Known limits:
//!
//! - The scan does not check WebSocket frames after a protocol upgrade.
//! - The scan does not find an encoded form of a token (a JSON `\u`
//!   escape, base64, URL encoding). The sandbox can make the proxy send a
//!   real token upstream only in the swapped credential headers. Thus the
//!   sandbox cannot make an upstream that echoes a request encode a token.

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

/// Maximum length of a token for the shape search. The search cuts longer
/// runs at this length (a JWT of the providers is a few KiB).
pub const MAX_TOKEN_LEN: usize = 8 * 1024;

/// How much of a long run of token characters the scanner holds back (at
/// least [`MAX_TOKEN_LEN`]). A run of up to twice this length is held
/// back fully.
pub const HOLD_MAX: usize = 32 * 1024;

/// How much of a JSON answer the scan reads before the answer goes to the
/// guest. A JSON answer of up to this size with a real token becomes a
/// local `502`, not a cut stream.
pub const HOLD_JSON: usize = 64 * 1024;

/// The scan does not search for store tokens shorter than this. A short
/// token can match by chance too easily.
const MIN_KNOWN_LEN: usize = 8;

/// The scan does not search for injected masked secrets shorter than
/// this. A user secret can be short and common (masked secrets have 8 or
/// more characters), and a chance match would cut an answer.
pub const MIN_INJECTED_LEN: usize = 16;

/// Whether `c` is a character of a token run: base64url, dots and `~`.
///
/// The providers' token shapes use no other characters. `=`, `/` and `+`
/// end a run, so a token after `name=` (a cookie) or in a URL path starts
/// its own run.
pub fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~')
}

/// Collect the real values that an API answer must not contain: the real
/// tokens of the service's store, and the real values of the masked
/// secrets injected into the request.
/// Args:
///  - `snapshot`: The service's grants
///  - `injected`: Masked secrets that the inject rules put into the
///    request.
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

/// Error: the scan found a real token.
#[derive(Debug)]
pub struct Found;

impl std::fmt::Display for Found {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("airlock: the answer carries a real token; the proxy ended it")
    }
}

impl std::error::Error for Found {}

/// What the scanner searches for, and where the last search stopped.
struct State {
    formats: &'static Formats,
    known: Vec<Vec<u8>>,
    /// Search for the token shapes (not in event streams).
    shapes: bool,
    /// The byte before the held bytes (the last byte sent to the guest).
    before: Option<u8>,
    /// Offset in the held bytes from which the candidate starts are not
    /// decided yet.
    shapes_from: usize,
    /// End of the known part of the run of the undecided candidate at
    /// `shapes_from`.
    open_end: Option<usize>,
    /// Offset in the held bytes from which known values can still start.
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

    /// Get the length of the longest end of `buf` that is the start of a
    /// known value (but not the full value). These bytes must wait for the
    /// next chunk.
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

    /// Search `buf` (the held bytes) from where the last search stopped.
    ///
    /// Bounded work: each byte is checked once for the start of a
    /// candidate token, and a candidate is at most [`MAX_TOKEN_LEN`] bytes.
    /// The next search does not search held bytes again.
    /// Args:
    ///  - `buf`: The held bytes
    ///  - `last`: True if no more data follows.
    ///
    /// Returns:
    ///   [`Found`] if `buf` contains a known value or a real token shape.
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
            // A run starts at `i`. It is the candidate, at most
            // MAX_TOKEN_LEN bytes. The shape search looks only at the start
            // of a run.
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

    /// Update the offsets after the first `n` held bytes went to the guest.
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
///
/// The primary check is an exact search for the real values that the
/// proxy knows (opaque tokens too). The second check searches for the
/// provider's token shapes ([`super::tokens::Format::starts`] and
/// [`super::tokens::Format::shape`]), but not surrogates.
///
/// A token split across chunks is found as a full token. The scanner holds
/// back the run of token characters ([`is_token_char`]) at the end of a
/// chunk until it ends. It holds back a run of up to twice [`HOLD_MAX`]
/// bytes fully. Of a longer run, it holds back only the last [`HOLD_MAX`]
/// bytes, and the run goes on in parts. It also holds back an end of the
/// chunk that is the start of a known value. All bytes before go on
/// immediately, so an event stream, whose events end in a newline, has no
/// delay.
pub struct Scanner {
    state: State,
    /// Bytes held back: scanned, but they can be the start of a token.
    held: Vec<u8>,
}

impl Scanner {
    /// Make a scanner.
    /// Args:
    ///  - `formats`: Token formats of the provider
    ///  - `known`: Real values to search for
    ///  - `shapes`: Also search for the provider's token shapes.
    pub fn new(formats: &'static Formats, known: Vec<String>, shapes: bool) -> Self {
        Self {
            state: State::new(formats, known, shapes),
            held: Vec::new(),
        }
    }

    /// Scan the next chunk.
    /// Returns:
    ///   The bytes that can go to the guest now (possibly none), or
    ///   [`Found`].
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

    /// Scan the held bytes as complete at the end of the body.
    /// Returns:
    ///   The held bytes, or [`Found`].
    pub fn finish(&mut self) -> Result<Bytes, Found> {
        self.state.check(&self.held, true)?;
        Ok(Bytes::from(std::mem::take(&mut self.held)))
    }

    /// Whether a complete value (a header or trailer value) contains a real
    /// value or a token shape. The shape search applies for all content
    /// types.
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

/// Get the media type of the `Content-Type`, lowercase, without
/// parameters.
fn mime_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| ct.split(';').next())
        .map(|mime| mime.trim().to_ascii_lowercase())
}

/// Check an answer of an API host for real tokens.
///
/// The checks:
///  * A switch of protocols (`101`, the WebSocket upgrade) passes.
///  * A compressed answer is refused (local `502`). The scan cannot read
///    it, and the proxy asked for no compression (`Accept-Encoding:
///    identity`).
///  * A response header that contains a real token refuses the answer.
///    This includes `Set-Cookie`: a cookie with a JWT with an OpenAI claim
///    makes the answer a `502`.
///  * The body and its trailers stream through a [`Scanner`]. An event
///    stream (model output) gets only the exact search, because model text
///    can contain strings with a token shape.
///  * A JSON answer is held until [`HOLD_JSON`] bytes are scanned. A hit
///    there gives a local `502`. Other answers go on immediately (also the
///    status). A hit then ends the stream with an error, so the chunk with
///    the token never gets to the guest.
///
/// Args:
///  - `resp`: The answer of the API host
///  - `formats`: Token formats of the provider
///  - `known`: Real values that the answer must not contain
///    ([`known_reals`]).
///
/// Returns:
///   The scanned answer, or a local `502`.
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

/// Make the local `502` answer for an answer with a real token.
fn refused() -> Response<ResponseBody> {
    server_error("an API answer that carries a real token")
}

/// A body that passes through a [`Scanner`].
struct ScanBody {
    inner: ResponseBody,
    scanner: Scanner,
    /// Frames that are ready for the guest.
    ready: VecDeque<Frame<Bytes>>,
    /// True if the inner body ended. Its remaining data is in `ready`.
    done: bool,
    /// True if the scan found a token. The stream ended with an error.
    failed: bool,
}

impl ScanBody {
    /// Scan one frame of the inner body, and queue the bytes that can go
    /// on.
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
    //! The stream scan of API answers: real token shapes, known values,
    //! tokens split across chunks, long runs and scan time.

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;
    use crate::services::{anthropic, openai};
    use crate::test_cfg::services::shaped_token;

    /// A scanner for `formats` and the `known` real values. `shapes` turns
    /// on the search for token shapes.
    fn scanner(formats: &'static Formats, known: &[&str], shapes: bool) -> Scanner {
        Scanner::new(
            formats,
            known.iter().map(ToString::to_string).collect(),
            shapes,
        )
    }

    /// Push `chunks` through `scanner` and finish it.
    /// Returns:
    ///   The output text, or `None` if the scanner found a real value.
    fn run(scanner: &mut Scanner, chunks: &[&str]) -> Option<String> {
        let mut out = Vec::new();
        for c in chunks {
            out.extend_from_slice(&scanner.push(c.as_bytes()).ok()?);
        }
        out.extend_from_slice(&scanner.finish().ok()?);
        Some(String::from_utf8(out).unwrap())
    }

    /// Test that the scan finds real Anthropic token shapes, but lets
    /// surrogates and strings that only look like tokens pass unchanged.
    ///   1. Scan a JSON value, a bearer header and a bare API key with real
    ///      token shapes and check that each is found
    ///   2. Scan a surrogate, a short key, a key inside a longer word, a wrong
    ///      prefix and plain text
    ///   3. Check that each of them passes unchanged
    #[test]
    fn real_token_shape_is_found_and_lookalikes_pass() {
        let a = &anthropic::FORMATS;
        for text in [
            format!(r#"{{"k":"{}"}}"#, shaped_token("sk-ant-oat01")),
            format!("Bearer {}", shaped_token("sk-ant-ort01")),
            shaped_token("sk-ant-api03"),
        ] {
            assert!(
                run(&mut scanner(a, &[], true), &[&text]).is_none(),
                "{text}"
            );
        }
        for text in [
            format!(r#"{{"k":"sk-ant-oat01-airlock-{}"}}"#, "s".repeat(64)),
            "sk-ant-api03-short example in a text".to_string(),
            format!("x{}", shaped_token("sk-ant-api03")),
            format!("assert_{}", shaped_token("sk-ant-api03")),
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

    /// Test that a token split across chunks is found before any of its bytes
    /// go out, and that a split surrogate passes.
    ///   1. Push text and the first part of a real token, and check that only
    ///      the text before the token goes out
    ///   2. Push the second part and check that nothing goes out
    ///   3. Push the end of the run and check that the scan fails
    ///   4. Scan a surrogate split in two and check that it passes unchanged
    #[test]
    fn token_split_across_chunks_is_found_before_its_bytes_go_out() {
        let token = shaped_token("sk-ant-oat01");
        let (x, y) = token.split_at(20);
        let mut s = scanner(&anthropic::FORMATS, &[], true);
        assert_eq!(
            &s.push(format!("data: one\n\ndata: {x}").as_bytes())
                .unwrap()[..],
            b"data: one\n\ndata: "
        );
        assert!(s.push(y.as_bytes()).unwrap().is_empty());
        assert!(s.push(b"\n\n").is_err());

        let surrogate = format!("sk-ant-oat01-airlock-{}", "s".repeat(64));
        let (x, y) = surrogate.split_at(15);
        let mut s = scanner(&anthropic::FORMATS, &[], true);
        assert_eq!(
            run(&mut s, &[x, y, "\n"]).unwrap(),
            format!("{surrogate}\n")
        );
    }

    /// Test that a known real value split across chunks is found, also when it
    /// contains characters that end a token run.
    ///   1. Scan a split opaque token and a split value with a space and a `!`
    ///   2. Check that each is found
    ///   3. Check that a value with only a common start passes
    #[test]
    fn known_value_split_across_chunks_is_found_even_with_run_breaking_characters() {
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

    /// Test that the scan finds an OpenAI JWT, also split or with text after
    /// it, but lets fake JWTs, other issuers and refresh-like strings pass.
    ///   1. Scan a split real JWT and a real JWT with more text after a dot
    ///   2. Check that both are found
    ///   3. Scan a fake JWT, a JWT of another issuer and two strings that look
    ///      like refresh tokens, and check that they pass unchanged
    #[test]
    fn openai_issuer_jwt_is_found_and_fakes_or_other_issuers_pass() {
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

    /// Test that a long run of token characters goes out in parts, so the
    /// scanner does not hold the full answer, and that the scan finds a token
    /// after a space.
    ///   1. Push a run longer than twice the hold limit
    ///   2. Check that all but the hold limit goes out
    ///   3. Push a real token to a new scanner and check that it is found
    #[test]
    fn long_run_goes_out_in_parts_and_token_after_it_is_found() {
        let mut s = scanner(&anthropic::FORMATS, &[], true);
        let long = "a".repeat(2 * HOLD_MAX + 100);
        assert_eq!(s.push(long.as_bytes()).unwrap().len(), HOLD_MAX + 100);
        let mut s = scanner(&anthropic::FORMATS, &[], true);
        assert!(
            s.push(format!("x {}\n", shaped_token("sk-ant-oat01")).as_bytes())
                .is_err()
        );
    }

    /// Test that answers made to slow the scan down are scanned in bounded
    /// time. A malicious upstream must not stall the proxy.
    ///   1. Make 1 MiB answers of repeated token starts, and a 64 KiB answer
    ///      of `a` in one-byte chunks
    ///   2. Push each in small and large chunks
    ///   3. Check that all bytes go out and that each scan takes less than
    ///      one second
    #[test]
    fn adversarial_runs_are_scanned_in_bounded_time() {
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
            // One-byte chunks are slow, so the `a` case scans less data.
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

    /// Test that a header value holds a real value if it contains a real token
    /// shape or a known value, but not a surrogate or a normal value.
    ///   1. Check a header with a real API key and a cookie with a known value
    ///   2. Check a surrogate and a content type
    #[test]
    fn header_value_with_real_token_or_known_value_holds_real() {
        let s = scanner(&anthropic::FORMATS, &["known-real-value"], false);
        assert!(s.holds_real(format!("x {}", shaped_token("sk-ant-api03")).as_bytes()));
        assert!(s.holds_real(b"a=known-real-value; Path=/"));
        assert!(!s.holds_real(b"sk-ant-api03-airlock-abcdefghijklmnopqrstu"));
        assert!(!s.holds_real(b"text/event-stream"));
    }
}
