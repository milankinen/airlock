//! Masked secret replacement in HTTP messages.
//!
//! The guest has only a surrogate for each masked `[env]` variable. For rules
//! that inject the variable, the proxy replaces the surrogate with the real
//! value in request headers. It also replaces the real value with the
//! surrogate in response headers, response bodies and other text. Thus the
//! real secret never goes into the VM.
//!
//! Known limits:
//!
//! - The body replacement does not see into compressed bodies. Thus the
//!   proxy asks the upstream for an uncompressed answer, and refuses a
//!   compressed answer.
//! - The replacement does not find an encoded form of a real value (a JSON
//!   `\u` escape, base64, URL encoding).
//! - The replacement does not check the bytes after a protocol upgrade.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Either};
use hyper::body::{Body, Frame};
use hyper::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, HeaderMap, HeaderValue};
use hyper::{Response, StatusCode};

use crate::network::http::{BoxError, ResponseBody, text_response};
use crate::network::target::InjectedSecret;

/// Replace surrogates with real values in all request header values.
/// Call it before Lua middleware runs.
/// Returns:
///   Error if a changed value is not a valid header value.
pub fn unmask_request(headers: &mut HeaderMap, secrets: &[InjectedSecret]) -> anyhow::Result<()> {
    rewrite_headers(headers, &unmask_pairs(secrets))
}

/// Replace real values with surrogates in all response header values.
/// Call it after Lua middleware runs.
/// Returns:
///   Error if a changed value is not a valid header value.
pub fn mask_response(headers: &mut HeaderMap, secrets: &[InjectedSecret]) -> anyhow::Result<()> {
    rewrite_headers(headers, &mask_pairs(secrets))
}

/// Ask the upstream for an uncompressed answer, so that [`mask_body`] can
/// see the real values in it. Call it after Lua middleware runs, so that a
/// script cannot change it.
pub fn request_identity(headers: &mut HeaderMap, secrets: &[InjectedSecret]) {
    if !secrets.is_empty() {
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    }
}

/// Replace real values with surrogates in the response body, while it
/// streams. Call it after Lua middleware runs. A real value can be split
/// across two chunks. The body holds back a short tail of each chunk to
/// find it. A surrogate of a masked `[env]` value has the same length as its
/// real value, so the body length usually does not change.
/// Returns:
///   The response with the masked body. A local 502 if the body is
///   compressed, because a compressed real value cannot be found.
pub fn mask_body(
    mut resp: Response<ResponseBody>,
    secrets: &[InjectedSecret],
) -> Response<ResponseBody> {
    let masker = Masker::new(secrets);
    if masker.pairs.is_empty() || resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        return resp;
    }
    if resp.body().is_end_stream() {
        return resp;
    }
    if resp
        .headers()
        .get_all(CONTENT_ENCODING)
        .iter()
        .any(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return text_response(
            StatusCode::BAD_GATEWAY,
            "compressed answer to a request with injected secrets\n",
        );
    }
    if !masker.same_length {
        // The masked body can have a different length. hyper then sends
        // the body without a declared length.
        resp.headers_mut().remove(CONTENT_LENGTH);
    }
    resp.map(|inner| {
        let body = MaskBody {
            inner,
            masker,
            ready: VecDeque::new(),
            done: false,
        };
        Either::Left(Either::Right(body.boxed_unsync()))
    })
}

/// Stream replacement of real values with surrogates.
struct Masker {
    /// `(real, surrogate)` pairs, longest real value first.
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
    /// Length of the longest real value.
    longest: usize,
    /// True if each surrogate has the length of its real value.
    same_length: bool,
    /// Bytes that are not checked yet.
    pending: Vec<u8>,
}

impl Masker {
    fn new(secrets: &[InjectedSecret]) -> Self {
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = mask_pairs(secrets)
            .into_iter()
            .filter(|(from, _)| !from.is_empty())
            .map(|(from, to)| (from.to_vec(), to.to_vec()))
            .collect();
        let longest = pairs.first().map_or(0, |(from, _)| from.len());
        let same_length = pairs.iter().all(|(from, to)| from.len() == to.len());
        Self {
            pairs,
            longest,
            same_length,
            pending: Vec::new(),
        }
    }

    /// Add `data` to the stream.
    /// Returns:
    ///   The masked bytes that can go on. A tail that can be the start of a
    ///   real value stays back.
    fn push(&mut self, data: &[u8]) -> Bytes {
        self.pending.extend_from_slice(data);
        self.drain(self.longest - 1)
    }

    /// End the stream.
    /// Returns:
    ///   The masked rest of the stream.
    fn finish(&mut self) -> Bytes {
        self.drain(0)
    }

    /// Mask and remove the pending bytes, except the last `keep` bytes.
    fn drain(&mut self, keep: usize) -> Bytes {
        let buf = &self.pending;
        // A match that starts before `end` fits in `buf` completely, because
        // `keep` is one less than the longest real value.
        let end = buf.len().saturating_sub(keep);
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        'scan: while i < end {
            // The longest match at a position wins, as in the header swap.
            for (from, to) in &self.pairs {
                if buf[i..].starts_with(from) {
                    out.extend_from_slice(to);
                    i += from.len();
                    continue 'scan;
                }
            }
            out.push(buf[i]);
            i += 1;
        }
        self.pending.drain(..i);
        Bytes::from(out)
    }
}

/// A body that passes through a [`Masker`].
struct MaskBody {
    inner: ResponseBody,
    masker: Masker,
    /// Frames that are ready for the guest.
    ready: VecDeque<Frame<Bytes>>,
    /// True if the inner body ended. Its remaining data is in `ready`.
    done: bool,
}

impl MaskBody {
    /// Mask one frame of the inner body, and queue the bytes that can go
    /// on. `None` is the end of the inner body.
    fn mask(&mut self, frame: Option<Frame<Bytes>>) -> anyhow::Result<()> {
        let frame = match frame.map(Frame::into_data) {
            Some(Ok(data)) => {
                let out = self.masker.push(&data);
                if !out.is_empty() {
                    self.ready.push_back(Frame::data(out));
                }
                return Ok(());
            }
            Some(Err(frame)) => Some(frame),
            None => None,
        };
        self.done = true;
        let rest = self.masker.finish();
        if !rest.is_empty() {
            self.ready.push_back(Frame::data(rest));
        }
        if let Some(frame) = frame {
            let frame = match frame.into_trailers() {
                Ok(mut trailers) => {
                    let pairs: Vec<(&[u8], &[u8])> = self
                        .masker
                        .pairs
                        .iter()
                        .map(|(from, to)| (from.as_slice(), to.as_slice()))
                        .collect();
                    rewrite_headers(&mut trailers, &pairs)?;
                    Frame::trailers(trailers)
                }
                Err(frame) => frame,
            };
            self.ready.push_back(frame);
        }
        Ok(())
    }
}

impl Body for MaskBody {
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
            if this.done {
                return Poll::Ready(None);
            }
            let frame = match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => None,
                Poll::Ready(Some(Ok(frame))) => Some(frame),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
            };
            let ended = frame.is_some() && this.inner.is_end_stream();
            let mut result = this.mask(frame);
            // End at once after the last frame, as the inner body does. hyper
            // then ends the message without one more poll.
            if ended && result.is_ok() && !this.done {
                result = this.mask(None);
            }
            if let Err(e) = result {
                this.done = true;
                return Poll::Ready(Some(Err(e.into())));
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done && self.ready.is_empty()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        if self.masker.same_length {
            self.inner.size_hint()
        } else {
            hyper::body::SizeHint::default()
        }
    }
}

/// Replace each real value in free-form `text` with its surrogate. Use it
/// for text that goes into the guest but is not a header. For example, the
/// body of an error response can quote a request header that was already
/// unmasked.
pub fn mask_text(text: &str, secrets: &[InjectedSecret]) -> String {
    let mut current = text.as_bytes().to_vec();
    for (from, to) in mask_pairs(secrets) {
        if from.is_empty() {
            continue;
        }
        if let Some(replaced) = replace_bytes(&current, from, to) {
            current = replaced;
        }
    }
    String::from_utf8_lossy(&current).into_owned()
}

fn unmask_pairs(secrets: &[InjectedSecret]) -> Vec<(&[u8], &[u8])> {
    ordered_pairs(
        secrets
            .iter()
            .map(|s| (s.surrogate.as_bytes(), s.real.as_bytes())),
    )
}

fn mask_pairs(secrets: &[InjectedSecret]) -> Vec<(&[u8], &[u8])> {
    ordered_pairs(
        secrets
            .iter()
            .map(|s| (s.real.as_bytes(), s.surrogate.as_bytes())),
    )
}

/// Sort the replacement pairs with the longest needle first.
///
/// The value of one secret can contain the value of another secret
/// (`AUTH_HEADER = "Bearer ${TOKEN}"` and `TOKEN`). If the shorter one is
/// replaced first, the longer match is broken, and the remaining part of
/// the longer value stays. If the longer one is replaced first, the result
/// does not depend on the order of the `inject` list.
fn ordered_pairs<'a>(
    pairs: impl Iterator<Item = (&'a [u8], &'a [u8])>,
) -> Vec<(&'a [u8], &'a [u8])> {
    let mut pairs: Vec<_> = pairs.collect();
    pairs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    pairs
}

/// Replace each `from` with its `to` in all header values, in the given
/// order. The replacement is a byte-level search and replace on all header
/// values (also repeated headers, `cookie` and `host`). Header names, the
/// URI and the body do not change. Empty `from` needles are skipped.
/// Args:
///  - `headers`: Headers to change
///  - `pairs`: `(from, to)` replacement pairs.
///
/// Returns:
///   Error if a changed value is not a valid header value. The error names
///   the header, but never shows its content.
pub fn rewrite_headers(headers: &mut HeaderMap, pairs: &[(&[u8], &[u8])]) -> anyhow::Result<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    for (name, value) in headers.iter_mut() {
        // Make a new header value only if something matched.
        let mut current: Option<Vec<u8>> = None;
        for (from, to) in pairs {
            if from.is_empty() {
                continue;
            }
            let haystack: &[u8] = current.as_deref().unwrap_or(value.as_bytes());
            if let Some(replaced) = replace_bytes(haystack, from, to) {
                current = Some(replaced);
            }
        }
        if let Some(bytes) = current {
            *value = HeaderValue::from_bytes(&bytes)
                .map_err(|_| anyhow::anyhow!("header `{name}`: rewritten value is not valid"))?;
        }
    }
    Ok(())
}

/// Replace all non-overlapping occurrences of `needle` in `haystack`.
/// Returns:
///   The new bytes, or `None` if nothing matched. Then the caller does not
///   need a new allocation.
fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Option<Vec<u8>> {
    debug_assert!(!needle.is_empty());
    let mut out: Option<Vec<u8>> = None;
    let mut last = 0;
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            let out = out.get_or_insert_with(|| Vec::with_capacity(haystack.len()));
            out.extend_from_slice(&haystack[last..i]);
            out.extend_from_slice(replacement);
            i += needle.len();
            last = i;
        } else {
            i += 1;
        }
    }
    let mut out = out?;
    out.extend_from_slice(&haystack[last..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    //! Tests for the swap of surrogates and real values in headers and
    //! streamed bodies.

    use super::*;

    /// A secret with the given name, real value and surrogate.
    fn secret(name: &str, real: &str, surrogate: &str) -> InjectedSecret {
        InjectedSecret::new(crate::project::MaskedSecret {
            name: name.into(),
            real: real.into(),
            surrogate: surrogate.into(),
        })
    }

    /// A header map with one header.
    fn header(name: &'static str, value: &[u8]) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_bytes(value).unwrap());
        h
    }

    /// Test that masking replaces the longer real value first when one
    /// secret contains another. Else a part of the longer real value stays
    /// in the header.
    ///   1. Make a token secret and an auth secret that contains the token
    ///   2. Mask a header with the auth value, with both secret orders
    ///   3. Check that the header has the auth surrogate only
    #[test]
    fn nested_secrets_mask_longer_value_regardless_of_order() {
        let token = secret("TOKEN", "real-token-value", "SURROGATE1234567");
        let auth = secret("AUTH", "Bearer real-token-value", "SURROGATEabcdefghijklmn");
        for order in [vec![token.clone(), auth.clone()], vec![auth, token]] {
            let mut h = header("x-echo", b"got Bearer real-token-value back");
            mask_response(&mut h, &order).unwrap();
            assert_eq!(h["x-echo"], "got SURROGATEabcdefghijklmn back");
        }
    }

    /// Test that a real value with non-ASCII bytes goes through the header
    /// swap in both directions. Header values are bytes, not ASCII text.
    ///   1. Unmask a request header with the surrogate
    ///   2. Check that it has the non-ASCII real value
    ///   3. Mask a response header with the real value
    ///   4. Check that it has the surrogate
    #[test]
    fn non_ascii_real_value_round_trips_through_headers() {
        let s = secret("TOKEN", "🔑-secret-token", "SURROGATEabcdef");
        let mut h = header("authorization", b"Bearer SURROGATEabcdef");
        unmask_request(&mut h, std::slice::from_ref(&s)).unwrap();
        assert_eq!(
            h["authorization"].as_bytes(),
            "Bearer 🔑-secret-token".as_bytes()
        );

        let mut h = header("x-echo", "got 🔑-secret-token back".as_bytes());
        mask_response(&mut h, &[s]).unwrap();
        assert_eq!(h["x-echo"], "got SURROGATEabcdef back");
    }

    /// Test that the stream masking gives the same result as the masking of
    /// the full text, for each split of the text into two chunks. A real
    /// value can arrive split at any byte.
    ///   1. Make a token secret and an auth secret that contains the token
    ///   2. Push a text with both values in two chunks, for each split point
    ///   3. Check that the output is the same as the full-text masking
    #[test]
    fn stream_masking_finds_values_split_at_each_byte() {
        let secrets = [
            secret("TOKEN", "real-token-value", "SURROGATE1234567"),
            secret("AUTH", "Bearer real-token-value", "SURROGATEabcdefghijklmn"),
        ];
        let text = "x real-token-value y Bearer real-token-value z real-token-valu";
        let expected = mask_text(text, &secrets);
        assert!(!expected.contains("real-token-value"), "{expected}");
        for cut in 0..=text.len() {
            let mut masker = Masker::new(&secrets);
            let mut out = masker.push(&text.as_bytes()[..cut]).to_vec();
            out.extend_from_slice(&masker.push(&text.as_bytes()[cut..]));
            out.extend_from_slice(&masker.finish());
            assert_eq!(String::from_utf8(out).unwrap(), expected, "cut {cut}");
        }
    }

    /// Test that a real value that is not valid in a header gives an error
    /// that does not show the real value. The error text can go to logs and
    /// to the guest.
    ///   1. Make a secret whose real value has a line break
    ///   2. Unmask a header with the surrogate
    ///   3. Check that the error names the header but not the real value
    #[test]
    fn real_value_invalid_in_header_errors_without_leaking() {
        let s = secret("TOKEN", "bad\r\nvalue-secret", "SURROGATE1234567");
        let mut h = header("authorization", b"SURROGATE1234567");
        let err = unmask_request(&mut h, &[s]).unwrap_err().to_string();
        assert!(err.contains("authorization"), "{err}");
        assert!(!err.contains("value-secret"), "{err}");
    }
}
