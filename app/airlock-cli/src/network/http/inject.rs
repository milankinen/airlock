//! Masked-secret substitution in HTTP headers.
//!
//! The guest only ever holds a surrogate for a masked `[env]` variable. For
//! rules that `inject` it, the proxy swaps the surrogate for the real value
//! in outbound request header values (before Lua middleware runs) and swaps
//! the real value back to the surrogate in response header values (after
//! Lua middleware has run), so the real secret never crosses into the VM.
//!
//! Rewriting is a byte-level search/replace over every header value —
//! including repeated headers, `cookie`, `host` and friends — and touches
//! nothing else (names, URI, body).

use hyper::header::{HeaderMap, HeaderValue};

use crate::network::target::InjectedSecret;

/// Request direction: surrogate → real.
pub fn unmask_request(headers: &mut HeaderMap, secrets: &[InjectedSecret]) -> anyhow::Result<()> {
    rewrite_headers(headers, &unmask_pairs(secrets))
}

/// Response direction: real → surrogate.
pub fn mask_response(headers: &mut HeaderMap, secrets: &[InjectedSecret]) -> anyhow::Result<()> {
    rewrite_headers(headers, &mask_pairs(secrets))
}

/// Replace every real value in free-form `text` with its surrogate. Used
/// for anything that is about to cross into the guest but is not a header
/// — e.g. the body of an error response, which may quote a request header
/// that was already unmasked.
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

/// Longest needle first. When one secret's value contains another's
/// (`AUTH_HEADER = "Bearer ${TOKEN}"` next to `TOKEN`), rewriting the
/// shorter one first would destroy the longer match and leave the rest of
/// the longer value in place; replacing the longer one first makes the
/// result independent of `inject` list order.
fn ordered_pairs<'a>(
    pairs: impl Iterator<Item = (&'a [u8], &'a [u8])>,
) -> Vec<(&'a [u8], &'a [u8])> {
    let mut pairs: Vec<_> = pairs.collect();
    pairs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    pairs
}

/// Replace every occurrence of each `from` with its `to` in every header
/// value, in the given order. Empty `from` needles are skipped. A header
/// value is only rebuilt when something actually matched; if the rebuilt
/// bytes are not a valid header value the error names the header but never
/// its content.
pub fn rewrite_headers(headers: &mut HeaderMap, pairs: &[(&[u8], &[u8])]) -> anyhow::Result<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    for (name, value) in headers.iter_mut() {
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
/// Returns `None` when nothing matched so callers can skip re-allocation.
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
    use super::*;

    fn secret(name: &str, real: &str, surrogate: &str) -> InjectedSecret {
        InjectedSecret::new(crate::project::MaskedSecret {
            name: name.into(),
            real: real.into(),
            surrogate: surrogate.into(),
        })
    }

    fn header(name: &'static str, value: &[u8]) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_bytes(value).unwrap());
        h
    }

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

    #[test]
    fn real_value_invalid_in_header_errors_without_leaking() {
        let s = secret("TOKEN", "bad\r\nvalue-secret", "SURROGATE1234567");
        let mut h = header("authorization", b"SURROGATE1234567");
        let err = unmask_request(&mut h, &[s]).unwrap_err().to_string();
        assert!(err.contains("authorization"), "{err}");
        assert!(!err.contains("value-secret"), "{err}");
    }
}
