//! Surrogate authorization codes.
//!
//! Makes sure that the real OAuth authorization code of a sign-in never
//! gets to the sandbox. The sandbox gets a surrogate code, and the host
//! changes it back to the real code when the agent redeems it. Also
//! records which sign-in pages the sandbox opened.
//!
//! The codes are only in the memory of this process.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::Mutex;
use sha2::{Digest as _, Sha256};
use url::Url;

use super::ServiceId;
use super::store::random_bytes;

/// Start of every surrogate code.
pub const PREFIX: &str = "airlock-code-";

/// How long a surrogate code or an opened sign-in page is valid.
const LIFETIME: Duration = Duration::from_mins(10);

/// Maximum number of codes (and of opened pages) per service. A new one
/// removes the oldest one of the service. A sign-in issues one code.
/// Because the limit is per service, a flood of codes of one service
/// cannot remove the codes of another service.
const MAX_PENDING: usize = 32;

/// The place where a real code came to the host.
///
/// A surrogate is bound to its channel. Thus another flow cannot use the
/// code of one sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// The browser's redirect to the loopback callback on this port. The
    /// callback forward ([`super::callback`]) replaces the `code` of each
    /// request's query.
    Callback(u16),
    /// The answer of a device-code poll (`authorization_code` of Codex's
    /// poll, see [`super::openai`]).
    Device,
}

impl Channel {
    /// Get the channel of an exchange's `redirect_uri`.
    /// Args:
    ///  - `redirect_uri`: The `redirect_uri` of the exchange
    ///  - `device_redirect`: The provider's device-flow redirect, if any.
    ///
    /// Returns:
    ///   [`Self::Device`] for `device_redirect`, [`Self::Callback`] with
    ///   the port for an `http` loopback URL, or `None` for anything else.
    pub fn of_redirect(redirect_uri: &str, device_redirect: Option<&str>) -> Option<Self> {
        if device_redirect.is_some_and(|d| d == redirect_uri) {
            return Some(Self::Device);
        }
        let url = Url::parse(redirect_uri).ok()?;
        let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        (url.scheme() == "http" && loopback)
            .then(|| url.port().map(Self::Callback))
            .flatten()
    }
}

/// The issued surrogate codes that are not used yet, and the opened
/// sign-in pages that no manual exchange used yet. Cheap to clone. Clones
/// share both.
///
/// The token exchange must contain a surrogate that this process issued.
/// The service puts the real code in its place, and refuses an unknown
/// code locally. A surrogate works once, for [`LIFETIME`], and only for
/// the same service and a `redirect_uri` of the same [`Channel`].
///
/// Claude's manual sign-in is the exception. The user pastes the real code
/// into the sandbox, so there is no surrogate to redeem. Its exchange is
/// bound to a sign-in page that the browser bridge opened
/// ([`Self::open_page`]): the exchange's PKCE `code_verifier` must hash
/// (S256) to the `code_challenge` of such a page. An opened page works for
/// one exchange and for [`LIFETIME`].
#[derive(Clone, Default)]
pub struct PendingCodes {
    codes: Arc<Mutex<HashMap<String, Pending>>>,
    pages: Arc<Mutex<Vec<OpenedPage>>>,
}

/// An issued surrogate code.
struct Pending {
    real: String,
    service: ServiceId,
    channel: Channel,
    issued: Instant,
}

/// A sign-in page the browser bridge opened.
struct OpenedPage {
    /// The page's PKCE `code_challenge` (S256).
    challenge: String,
    service: ServiceId,
    opened: Instant,
}

impl PendingCodes {
    /// Issue a surrogate for the real code `real` that came to `service`
    /// through `channel`.
    pub fn issue(
        &self,
        real: &str,
        service: ServiceId,
        channel: Channel,
    ) -> anyhow::Result<String> {
        let surrogate = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(random_bytes::<32>()?));
        let mut codes = self.codes.lock();
        codes.retain(|_, p| p.issued.elapsed() < LIFETIME);
        while codes.values().filter(|p| p.service == service).count() >= MAX_PENDING {
            let oldest = codes
                .iter()
                .filter(|(_, p)| p.service == service)
                .min_by_key(|(_, p)| p.issued)
                .map(|(k, _)| k.clone())
                .expect("the service has codes");
            codes.remove(&oldest);
        }
        codes.insert(
            surrogate.clone(),
            Pending {
                real: real.to_string(),
                service,
                channel,
                issued: Instant::now(),
            },
        );
        Ok(surrogate)
    }

    /// Redeem `surrogate` once, if it was issued to `service` through
    /// `channel`. The surrogate is used in all cases.
    /// Returns:
    ///   The real code, or `None` for an unknown, used, expired or foreign
    ///   surrogate.
    pub fn redeem(&self, surrogate: &str, service: ServiceId, channel: Channel) -> Option<String> {
        let pending = self.codes.lock().remove(surrogate)?;
        (pending.issued.elapsed() < LIFETIME
            && pending.service == service
            && pending.channel == channel)
            .then_some(pending.real)
    }

    /// Record that the browser bridge opened a sign-in page of `service`
    /// with the PKCE `code_challenge` `challenge`.
    pub fn open_page(&self, challenge: &str, service: ServiceId) {
        let mut pages = self.pages.lock();
        pages.retain(|p| p.opened.elapsed() < LIFETIME);
        while pages.iter().filter(|p| p.service == service).count() >= MAX_PENDING {
            let oldest = pages
                .iter()
                .position(|p| p.service == service)
                .expect("the service has pages");
            pages.remove(oldest);
        }
        pages.push(OpenedPage {
            challenge: challenge.to_string(),
            service,
            opened: Instant::now(),
        });
    }

    /// Whether `verifier` is the PKCE verifier of a page opened for
    /// `service` in the last [`LIFETIME`] ([`Self::open_page`]): its S256
    /// challenge matches. A match uses the page.
    pub fn redeem_page(&self, verifier: &str, service: ServiceId) -> bool {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut pages = self.pages.lock();
        let found = pages.iter().position(|p| {
            p.service == service && p.challenge == challenge && p.opened.elapsed() < LIFETIME
        });
        found.map(|i| pages.remove(i)).is_some()
    }

    /// Replace the value of every `code` parameter of `query` with a
    /// surrogate issued to `service` through `channel`.
    /// Returns:
    ///   The changed query, or `None` if the query has no `code`. Then the
    ///   query stays unchanged, byte for byte.
    pub fn rewrite_query(
        &self,
        query: &str,
        service: ServiceId,
        channel: Channel,
    ) -> anyhow::Result<Option<String>> {
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if !pairs.iter().any(|(k, _)| k == "code") {
            return Ok(None);
        }
        let mut out = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &pairs {
            if k == "code" {
                out.append_pair(k, &self.issue(v, service, channel)?);
            } else {
                out.append_pair(k, v);
            }
        }
        Ok(Some(out.finish()))
    }
}

#[cfg(test)]
mod tests {
    //! Surrogate authorization codes and opened sign-in pages: binding to a
    //! service and channel, single use, expiry and flood limits.

    use super::*;

    const CALLBACK: Channel = Channel::Callback(1455);
    // The PKCE verifier and S256 challenge example from RFC 7636.
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    /// Test that a surrogate code redeems only on its own service and channel,
    /// and that a wrong redeem uses it up. Another flow must not get the real
    /// code of a sign-in.
    ///   1. Issue a code for one service and callback port
    ///   2. Redeem it on another service, port or flow and check that it fails
    ///   3. Redeem it on its own service and channel and check that it fails
    #[test]
    fn surrogate_redeemed_on_other_service_port_or_flow_fails_and_is_used_up() {
        let codes = PendingCodes::default();
        for (service, channel) in [
            (ServiceId::Anthropic, CALLBACK),
            (ServiceId::Openai, Channel::Callback(1457)),
            (ServiceId::Openai, Channel::Device),
        ] {
            let s = codes.issue("real", ServiceId::Openai, CALLBACK).unwrap();
            assert_eq!(codes.redeem(&s, service, channel), None);
            assert_eq!(codes.redeem(&s, ServiceId::Openai, CALLBACK), None);
        }
    }

    /// Test that a redirect URI gives the loopback callback port or the device
    /// flow, and that other URIs give no channel.
    ///   1. Get the channel of loopback, device, HTTPS, remote and bad URIs
    ///   2. Check that only `http` loopback URIs with a port and the device
    ///      redirect give a channel
    ///   3. Check that the device URI gives no channel when the provider has
    ///      no device flow
    #[test]
    fn redirect_uri_names_loopback_port_or_device_flow() {
        let device = Some("https://auth.openai.com/deviceauth/callback");
        for (uri, want) in [
            ("http://127.0.0.1:1455/auth/callback", Some(CALLBACK)),
            (
                "http://localhost:40000/callback",
                Some(Channel::Callback(40000)),
            ),
            ("http://[::1]:1455/x", Some(CALLBACK)),
            (
                "https://auth.openai.com/deviceauth/callback",
                Some(Channel::Device),
            ),
            ("https://127.0.0.1:1455/auth/callback", None),
            ("http://evil.com:1455/auth/callback", None),
            ("http://localhost/callback", None),
            ("not a url", None),
        ] {
            assert_eq!(Channel::of_redirect(uri, device), want, "{uri}");
        }
        assert_eq!(
            Channel::of_redirect("https://auth.openai.com/deviceauth/callback", None),
            None
        );
    }

    /// Test that a flood of codes removes the oldest code of the same service
    /// only. A flood on one service must not break a sign-in on another.
    ///   1. Issue a code for each service
    ///   2. Issue the maximum number of codes for the first service
    ///   3. Check that the oldest code of the first service is removed
    ///   4. Check that the code of the other service still redeems
    #[test]
    fn flood_of_codes_drops_oldest_code_of_same_service_only() {
        let codes = PendingCodes::default();
        let first = codes.issue("first", ServiceId::Openai, CALLBACK).unwrap();
        let other = codes
            .issue("other", ServiceId::Anthropic, Channel::Callback(40000))
            .unwrap();
        for _ in 0..MAX_PENDING {
            codes.issue("more", ServiceId::Openai, CALLBACK).unwrap();
        }
        assert_eq!(codes.redeem(&first, ServiceId::Openai, CALLBACK), None);
        assert_eq!(codes.codes.lock().len(), MAX_PENDING + 1);
        assert_eq!(
            codes
                .redeem(&other, ServiceId::Anthropic, Channel::Callback(40000))
                .as_deref(),
            Some("other")
        );
    }

    /// Test that an opened sign-in page redeems only with its PKCE verifier,
    /// only for its service, and only once.
    ///   1. Check that a verifier fails before the page is opened
    ///   2. Open the page with the S256 challenge
    ///   3. Check that a wrong verifier, the wrong service and the challenge
    ///      itself fail
    ///   4. Check that the correct verifier works once only
    #[test]
    fn opened_page_redeems_its_s256_verifier_once_for_its_service() {
        let codes = PendingCodes::default();
        assert!(!codes.redeem_page(VERIFIER, ServiceId::Anthropic));
        codes.open_page(CHALLENGE, ServiceId::Anthropic);
        assert!(!codes.redeem_page("another-verifier", ServiceId::Anthropic));
        assert!(!codes.redeem_page(VERIFIER, ServiceId::Openai));
        assert!(!codes.redeem_page(CHALLENGE, ServiceId::Anthropic));
        assert!(codes.redeem_page(VERIFIER, ServiceId::Anthropic));
        assert!(!codes.redeem_page(VERIFIER, ServiceId::Anthropic));
    }

    /// Test that an opened sign-in page expires after its lifetime.
    ///   1. Open a page and set its open time back by the lifetime
    ///   2. Check that its verifier does not redeem
    #[test]
    fn opened_page_expires_after_lifetime() {
        let codes = PendingCodes::default();
        codes.open_page(CHALLENGE, ServiceId::Anthropic);
        codes.pages.lock()[0].opened = Instant::now().checked_sub(LIFETIME).unwrap();
        assert!(!codes.redeem_page(VERIFIER, ServiceId::Anthropic));
    }

    /// Test that a flood of opened pages removes the oldest page of the same
    /// service only.
    ///   1. Open the same page for both services
    ///   2. Open the maximum number of pages for the first service
    ///   3. Check that the first service lost its page and the other kept it
    #[test]
    fn flood_of_pages_drops_oldest_page_of_same_service_only() {
        let codes = PendingCodes::default();
        codes.open_page(CHALLENGE, ServiceId::Anthropic);
        codes.open_page(CHALLENGE, ServiceId::Openai);
        for _ in 0..MAX_PENDING {
            codes.open_page("other", ServiceId::Anthropic);
        }
        assert!(!codes.redeem_page(VERIFIER, ServiceId::Anthropic));
        assert!(codes.redeem_page(VERIFIER, ServiceId::Openai));
    }
}
