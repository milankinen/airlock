//! Masked secret replacement in HTTP headers.
//!
//! The guest has only a surrogate for each masked `[env]` variable. For rules
//! that inject the variable, the proxy replaces the surrogate with the real
//! value in request headers. It also replaces the real value with the
//! surrogate in response headers and other text. Thus the real secret never
//! goes into the VM.

use hyper::header::{HeaderMap, HeaderValue};

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
    //! Tests for the swap of surrogates and real values in headers.

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
