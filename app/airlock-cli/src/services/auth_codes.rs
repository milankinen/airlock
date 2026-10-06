//! Surrogate authorization codes: the real OAuth authorization code of a
//! sign-in never reaches the sandbox.
//!
//! The code comes to the host in two ways, and both swap it for a
//! surrogate (`airlock-code-` and random characters) before the sandbox
//! gets it:
//!
//! - the browser's redirect to the loopback callback, which the
//!   service's sign-in forwards into the sandbox ([`super::callback`]
//!   rewrites the `code` of each request's query);
//! - the answer of Codex's device-code poll (`authorization_code`, see
//!   [`super::openai`]).
//!
//! The token exchange then must carry a surrogate this process issued:
//! the service swaps in the real code, and refuses an unknown code
//! locally. A surrogate is bound to where it was issued: its service and
//! its [`Channel`] (the callback port, or the device poll). An exchange
//! redeems it only for the same service and with a `redirect_uri` of the
//! same channel, so a code of one sign-in cannot be spent by another
//! service or flow. A surrogate works once and for [`LIFETIME`]. The codes
//! live in the memory of this process only; each service has its own
//! [`PendingCodes`] and keeps at most [`MAX_PENDING`], so a flood of one
//! service's codes cannot push out another's.
//!
//! Claude's manual sign-in is the exception: the user pastes the real
//! code into the sandbox, so there is no surrogate to redeem. Its
//! exchange is bound to a sign-in page the browser bridge opened instead
//! ([`PendingCodes::open_page`]): the exchange's PKCE `code_verifier` must
//! hash (S256) to the `code_challenge` of a page this process opened for
//! the service. An opened page works for one exchange and for
//! [`LIFETIME`]; the service keeps at most [`MAX_PENDING`].

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

/// What a surrogate code starts with.
pub const PREFIX: &str = "airlock-code-";

/// How long a surrogate code, and an opened sign-in page, works.
const LIFETIME: Duration = Duration::from_mins(10);

/// The most codes (and opened pages) kept per service; a new one drops
/// the service's oldest. A sign-in issues one.
const MAX_PENDING: usize = 32;

/// Where a real code came to the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// The browser's redirect to the loopback callback on this port.
    Callback(u16),
    /// The answer of a device-code poll.
    Device,
}

impl Channel {
    /// The channel of an exchange's `redirect_uri`: an `http` loopback URL
    /// names its port; `device_redirect` (the provider's device-flow
    /// redirect) is the device channel. `None` for anything else.
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

/// The surrogate codes issued and not yet used, and the sign-in pages
/// opened and not yet used by a manual exchange. Cheap to clone; clones
/// share both.
#[derive(Clone, Default)]
pub struct PendingCodes {
    codes: Arc<Mutex<HashMap<String, Pending>>>,
    pages: Arc<Mutex<Vec<OpenedPage>>>,
}

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
    /// A surrogate for the real code `real` that came to `service`
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

    /// The real code of `surrogate`, once, when it was issued to `service`
    /// through `channel`: the surrogate is used up either way. `None` for
    /// an unknown, used, expired or foreign surrogate.
    pub fn redeem(&self, surrogate: &str, service: ServiceId, channel: Channel) -> Option<String> {
        let pending = self.codes.lock().remove(surrogate)?;
        (pending.issued.elapsed() < LIFETIME
            && pending.service == service
            && pending.channel == channel)
            .then_some(pending.real)
    }

    /// Remember that the browser bridge opened a sign-in page of `service`
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
    /// `service` within [`LIFETIME`] ([`Self::open_page`]): its S256
    /// challenge matches. A match uses the page up.
    pub fn redeem_page(&self, verifier: &str, service: ServiceId) -> bool {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut pages = self.pages.lock();
        let found = pages.iter().position(|p| {
            p.service == service && p.challenge == challenge && p.opened.elapsed() < LIFETIME
        });
        found.map(|i| pages.remove(i)).is_some()
    }

    /// `query` with the value of every `code` parameter swapped for a
    /// surrogate issued to `service` through `channel`. `None` when the
    /// query has no `code`: then it stays as it is, byte for byte.
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
    use super::*;

    const CALLBACK: Channel = Channel::Callback(1455);

    #[test]
    fn a_surrogate_redeems_once() {
        let codes = PendingCodes::default();
        let s = codes
            .issue("real-code", ServiceId::Openai, CALLBACK)
            .unwrap();
        assert!(s.starts_with(PREFIX));
        assert_eq!(codes.redeem("real-code", ServiceId::Openai, CALLBACK), None);
        assert_eq!(
            codes.redeem(&s, ServiceId::Openai, CALLBACK).as_deref(),
            Some("real-code")
        );
        assert_eq!(codes.redeem(&s, ServiceId::Openai, CALLBACK), None);
    }

    /// A code of one service, port or flow is no code of another, and a
    /// failed try uses it up.
    #[test]
    fn a_surrogate_redeems_only_where_it_was_issued() {
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

    #[test]
    fn the_channel_of_a_redirect() {
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

    #[test]
    fn the_query_gets_a_surrogate_code() {
        let codes = PendingCodes::default();
        let q = codes
            .rewrite_query("code=real-code&state=a%20b", ServiceId::Openai, CALLBACK)
            .unwrap()
            .unwrap();
        assert!(!q.contains("real-code"), "{q}");
        let pairs: HashMap<String, String> = url::form_urlencoded::parse(q.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(pairs["state"], "a b");
        assert_eq!(
            codes
                .redeem(&pairs["code"], ServiceId::Openai, CALLBACK)
                .as_deref(),
            Some("real-code")
        );
        assert_eq!(
            codes
                .rewrite_query("state=x&error=denied", ServiceId::Openai, CALLBACK)
                .unwrap(),
            None
        );
    }

    /// Each service keeps its own [`MAX_PENDING`] codes: a flood of one
    /// drops its own oldest code only.
    #[test]
    fn old_codes_are_dropped_per_service() {
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

    /// RFC 7636, appendix B: the S256 challenge of the example verifier.
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    /// An opened page binds one manual exchange: its verifier, its
    /// service, once.
    #[test]
    fn an_opened_page_redeems_its_verifier_once() {
        let codes = PendingCodes::default();
        assert!(!codes.redeem_page(VERIFIER, ServiceId::Anthropic));
        codes.open_page(CHALLENGE, ServiceId::Anthropic);
        assert!(!codes.redeem_page("another-verifier", ServiceId::Anthropic));
        assert!(!codes.redeem_page(VERIFIER, ServiceId::Openai));
        assert!(!codes.redeem_page(CHALLENGE, ServiceId::Anthropic));
        assert!(codes.redeem_page(VERIFIER, ServiceId::Anthropic));
        assert!(!codes.redeem_page(VERIFIER, ServiceId::Anthropic));
    }

    #[test]
    fn an_opened_page_expires() {
        let codes = PendingCodes::default();
        codes.open_page(CHALLENGE, ServiceId::Anthropic);
        codes.pages.lock()[0].opened = Instant::now().checked_sub(LIFETIME).unwrap();
        assert!(!codes.redeem_page(VERIFIER, ServiceId::Anthropic));
    }

    /// Each service keeps its own [`MAX_PENDING`] opened pages.
    #[test]
    fn old_pages_are_dropped_per_service() {
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
