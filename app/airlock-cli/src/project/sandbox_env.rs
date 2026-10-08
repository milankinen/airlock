//! Resolution of the `[env]` section into the environment the guest sees.
//!
//! Every value template is substituted once through the project vault
//! (host env first, secret vault as fallback). Entries marked `mask = true`
//! additionally get a **surrogate**: an ASCII alphanumeric string with the
//! same byte length as the real value. The guest only ever receives the
//! surrogate; the real value stays on the host, where the network proxy can
//! swap it back into outbound HTTP headers for rules that `inject` it.
//!
//! Surrogates are stable: derived from the variable name and the value's
//! byte length, never from the value. Tools that persist the
//! credential on first run (Codex) keep working across restarts and
//! secret rotation.

use std::collections::BTreeMap;
use std::fmt;

use rand::rngs::ChaCha20Rng;
use rand::{Rng, SeedableRng};
use sha2::{Digest, Sha256};

use crate::config::config_values::EnvVar;
use crate::vault::Vault;

/// Shortest value a network rule may inject. A surrogate this short
/// (random alphanumerics) would plausibly appear inside unrelated header
/// text and the byte-level rewrite would corrupt it.
pub const MIN_INJECT_LEN: usize = 8;

/// A problem with one `[env]` entry. Kept as its own type so `airlock
/// start` can recognise it as a configuration error (exit code 2) even
/// when it surfaces from deep inside project setup.
#[derive(Debug, thiserror::Error)]
#[error("env.{name}: {reason}")]
pub struct EnvError {
    pub name: String,
    pub reason: String,
}

impl EnvError {
    fn new(name: &str, reason: impl fmt::Display) -> Self {
        Self {
            name: name.to_string(),
            reason: reason.to_string(),
        }
    }
}

/// A masked `[env]` entry: the real value (host-only) and the surrogate the
/// guest sees in its place.
#[derive(Clone)]
pub struct MaskedSecret {
    pub name: String,
    pub real: String,
    pub surrogate: String,
}

impl MaskedSecret {
    /// The masked entry `name` with the value `real` and its surrogate.
    pub fn new(name: &str, real: String) -> Self {
        Self {
            name: name.to_string(),
            surrogate: surrogate_for(name, &real),
            real,
        }
    }
}

// Manual Debug so a stray `{:?}` on a target or connection never prints the
// real value (or the surrogate, which is as good as the real one once the
// proxy is willing to swap it).
impl fmt::Debug for MaskedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MaskedSecret")
            .field("name", &self.name)
            .field("len", &self.real.len())
            .finish_non_exhaustive()
    }
}

/// The resolved sandbox environment.
///
/// No `Debug`: the guest values include substituted secrets for unmasked
/// entries, and the masked map holds the real ones.
#[derive(Clone)]
pub struct SandboxEnv {
    /// Every `[env]` entry in config order with the guest-visible value.
    guest: Vec<(String, String)>,
    /// The masked subset, keyed by variable name.
    masked: BTreeMap<String, MaskedSecret>,
}

impl SandboxEnv {
    /// Substitute every template and generate surrogates for masked entries.
    /// A template referencing an undefined host variable / vault secret is
    /// an [`EnvError`] naming the key.
    pub fn resolve(env: &BTreeMap<String, EnvVar>, vault: &Vault) -> Result<Self, EnvError> {
        let mut guest = Vec::with_capacity(env.len());
        let mut masked = BTreeMap::new();
        for (key, entry) in env {
            let real = vault
                .subst(&entry.value)
                .map_err(|e| EnvError::new(key, e))?;
            if entry.mask {
                let secret = MaskedSecret::new(key, real);
                guest.push((key.clone(), secret.surrogate.clone()));
                masked.insert(key.clone(), secret);
            } else {
                guest.push((key.clone(), real));
            }
        }
        Ok(Self { guest, masked })
    }

    /// An environment with no entries. Used by the read-only project
    /// loader, which must never trigger secret resolution.
    pub fn empty() -> Self {
        Self {
            guest: Vec::new(),
            masked: BTreeMap::new(),
        }
    }

    /// Build an environment consisting solely of the given masked secrets.
    /// Test helper for the network harness.
    #[cfg(test)]
    pub fn from_secrets(secrets: Vec<MaskedSecret>) -> Self {
        let guest = secrets
            .iter()
            .map(|s| (s.name.clone(), s.surrogate.clone()))
            .collect();
        let masked = secrets.into_iter().map(|s| (s.name.clone(), s)).collect();
        Self { guest, masked }
    }

    /// The value the guest sees for `name` — the surrogate for masked
    /// entries, the substituted value otherwise.
    pub fn guest_value(&self, name: &str) -> Option<&str> {
        self.guest
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// `(KEY, guest-visible VALUE)` pairs in config order.
    pub fn guest_entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.guest.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// The masked entry for `name`, if it exists and is masked.
    pub fn masked(&self, name: &str) -> Option<&MaskedSecret> {
        self.masked.get(name)
    }

    /// Check that `name` can be injected into HTTP headers: it must be a
    /// masked entry, at least [`MIN_INJECT_LEN`] characters long, and a
    /// valid header value (a stray newline from a `.env` loader would
    /// otherwise pass startup and break every request with an opaque 502).
    pub fn check_injectable(&self, name: &str) -> Result<(), EnvError> {
        let Some(secret) = self.masked.get(name) else {
            return Err(EnvError::new(
                name,
                "must be defined in [env] with mask = true to be injected",
            ));
        };
        if secret.real.len() < MIN_INJECT_LEN {
            return Err(EnvError::new(
                name,
                format!("injected value is shorter than {MIN_INJECT_LEN} bytes"),
            ));
        }
        if hyper::header::HeaderValue::from_str(&secret.real).is_err() {
            return Err(EnvError::new(
                name,
                "injected value contains bytes that are not allowed in an HTTP header \
                 (a newline or control character, perhaps a trailing newline)",
            ));
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.guest.len()
    }

    pub fn is_empty(&self) -> bool {
        self.guest.is_empty()
    }

    pub fn masked_count(&self) -> usize {
        self.masked.len()
    }
}

/// Domain separator. Bump the version if the derivation changes; cached
/// surrogates become invalid.
const SURROGATE_DOMAIN: &[u8] = b"airlock-surrogate-v1";

/// The surrogate alphabet, 62 symbols.
const SURROGATE_ALPHABET: &[u8; 62] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// A deterministic `[A-Za-z0-9]` string with the same byte length as
/// `value`, derived from `name` and that length only. The value itself
/// never enters the hash, so the surrogate reveals nothing but the length.
///
/// ChaCha20 seeded with a SHA-256 of the inputs. Its stream is stable
/// across crate versions, unlike `StdRng`.
fn surrogate_for(name: &str, value: &str) -> String {
    let len = value.len();
    let mut hasher = Sha256::new();
    hasher.update(SURROGATE_DOMAIN);
    hasher.update((name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    hasher.update((len as u64).to_le_bytes());
    let mut rng = ChaCha20Rng::from_seed(hasher.finalize().into());
    (0..len)
        .map(|_| SURROGATE_ALPHABET[(rng.next_u32() % 62) as usize] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_cfg::host_env_vault;

    #[test]
    fn surrogate_is_alphanumeric_with_same_byte_length() {
        for real in ["sk-ant-oat01-abcdefghijklmnop", "x", "", "ääkkönen-token"] {
            let s = surrogate_for("TOKEN", real);
            assert_eq!(s.len(), real.len(), "{real}");
            assert!(s.chars().all(|c| c.is_ascii_alphanumeric()), "{s}");
        }
    }

    #[test]
    fn surrogate_depends_only_on_name_and_length() {
        let a = surrogate_for("OPENAI_API_KEY", "sk-aaaaaaaaaaaaaaaaaaaa");
        assert_eq!(
            a,
            surrogate_for("OPENAI_API_KEY", "sk-bbbbbbbbbbbbbbbbbbbb")
        );
        assert_ne!(
            a,
            surrogate_for("OPENAI_API_KEX", "sk-aaaaaaaaaaaaaaaaaaaa")
        );
        assert_ne!(a, surrogate_for("OPENAI_API_KEY", "sk-aaaaaaaaaaaaaaaaaaa"));
        assert_ne!(surrogate_for("TOKEN1", "ab"), surrogate_for("TOKEN", "ab"));
        assert_ne!(
            surrogate_for("TOKEN1", "abcdefghij"),
            surrogate_for("TOKEN", "abcdefghij")
        );
    }

    #[test]
    fn surrogate_is_pinned() {
        assert_eq!(
            surrogate_for(
                "OPENAI_API_KEY",
                "sk-proj-0123456789abcdefghijklmnopqrstuvwxyz0123"
            ),
            "gHPoAQXAEPp1xt0XCGvsexx4AicTJokPIS7jw7D8AvRuM4BL"
        );
        assert_eq!(
            surrogate_for(
                "CLAUDE_CODE_OAUTH_TOKEN",
                "sk-ant-oat01-0123456789abcdefghijklmnopqrstuvwxyz"
            ),
            "4YIqU4HuWttkzVxUuWLXos9ycGwcCfUKlxnsofMlyjpKd4LBC"
        );
        assert_eq!(
            surrogate_for("LONG", &"x".repeat(100)),
            "csNmqNAJNbzpxubyNC7TSRONA5TTH97VhNl5WxXvbhDp23aR20Rwj0LDNCapy0BUKUbTb677RnbzvWeHfazQcvQ57sh8sOX1FBBX"
        );
    }

    #[test]
    fn check_injectable_refuses_unmasked_and_undefined_names() {
        let env = BTreeMap::from([(
            "PLAIN".to_string(),
            EnvVar::plain("sk-real-token-0123456789"),
        )]);
        let resolved = SandboxEnv::resolve(&env, &host_env_vault(&[])).unwrap();
        for name in ["PLAIN", "NOPE"] {
            let err = resolved.check_injectable(name).unwrap_err().to_string();
            assert_eq!(
                err,
                format!("env.{name}: must be defined in [env] with mask = true to be injected")
            );
        }
    }

    #[test]
    fn masked_secret_debug_never_prints_values() {
        let secret = MaskedSecret::new("TOKEN", "real-secret-value".into());
        let debug = format!("{secret:?}");
        assert!(debug.contains("TOKEN"));
        assert!(!debug.contains("real-secret-value"));
        assert!(!debug.contains(&secret.surrogate));
    }
}
