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
    use std::collections::HashMap;

    use super::*;
    use crate::vault::{DisabledStorage, VaultStorageType};

    fn vault(host_env: &[(&str, &str)]) -> Vault {
        Vault::new_with(
            Box::new(DisabledStorage),
            host_env
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<HashMap<_, _>>(),
            VaultStorageType::Disabled,
        )
    }

    fn env(entries: &[(&str, &str, bool)]) -> BTreeMap<String, EnvVar> {
        entries
            .iter()
            .map(|(k, v, mask)| {
                (
                    (*k).to_string(),
                    EnvVar {
                        value: (*v).to_string(),
                        mask: *mask,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn surrogate_has_same_byte_length_and_is_alphanumeric() {
        for real in ["sk-ant-oat01-abcdefghijklmnop", "x", "ääkkönen-token"] {
            let s = surrogate_for("TOKEN", real);
            assert_eq!(s.len(), real.len(), "for {real}");
            assert!(s.chars().all(|c| c.is_ascii_alphanumeric()), "got {s}");
        }
    }

    #[test]
    fn surrogate_of_empty_is_empty() {
        assert_eq!(surrogate_for("TOKEN", ""), "");
    }

    #[test]
    fn surrogate_differs_from_real_value() {
        let real = "sk-ant-oat01-abcdefghijklmnop";
        // The alphabet excludes `-`, so a collision is impossible here.
        assert_ne!(surrogate_for("TOKEN", real), real);
    }

    #[test]
    fn surrogate_depends_only_on_name_and_length() {
        // Same name and length, different value: same surrogate.
        let a = surrogate_for("OPENAI_API_KEY", "sk-aaaaaaaaaaaaaaaaaaaa");
        let b = surrogate_for("OPENAI_API_KEY", "sk-bbbbbbbbbbbbbbbbbbbb");
        assert_eq!(a, b);
        // Different name or different length: different surrogate.
        assert_ne!(
            a,
            surrogate_for("OPENAI_API_KEX", "sk-aaaaaaaaaaaaaaaaaaaa")
        );
        assert_ne!(a, surrogate_for("OPENAI_API_KEY", "sk-aaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn surrogate_name_and_length_do_not_alias() {
        // Name is length-prefixed, so name/length boundaries cannot shift.
        assert_ne!(surrogate_for("TOKEN1", "ab"), surrogate_for("TOKEN", "ab"));
        assert_ne!(
            surrogate_for("TOKEN1", "abcdefghij"),
            surrogate_for("TOKEN", "abcdefghij")
        );
    }

    #[test]
    fn surrogate_is_pinned() {
        // Golden values. A change here invalidates every cached surrogate:
        // bump `SURROGATE_DOMAIN` or revert.
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
        // Spans more than one ChaCha block.
        let long = surrogate_for("LONG", &"x".repeat(100));
        assert_eq!(long.len(), 100);
        assert_eq!(
            long,
            "csNmqNAJNbzpxubyNC7TSRONA5TTH97VhNl5WxXvbhDp23aR20Rwj0LDNCapy0BUKUbTb677RnbzvWeHfazQcvQ57sh8sOX1FBBX"
        );
    }

    #[test]
    fn unmasked_entries_pass_through_substituted() {
        let v = vault(&[("HOST_TOKEN", "real-value-1234")]);
        let e = env(&[
            ("PLAIN", "static", false),
            ("SUBST", "${HOST_TOKEN}", false),
        ]);
        let resolved = SandboxEnv::resolve(&e, &v).unwrap();
        assert_eq!(resolved.guest_value("PLAIN"), Some("static"));
        assert_eq!(resolved.guest_value("SUBST"), Some("real-value-1234"));
        assert_eq!(resolved.masked_count(), 0);
        assert!(resolved.masked("SUBST").is_none());
    }

    #[test]
    fn masked_entry_is_substituted_then_masked() {
        let v = vault(&[("HOST_TOKEN", "real-value-1234")]);
        let e = env(&[("TOKEN", "${HOST_TOKEN}", true)]);
        let resolved = SandboxEnv::resolve(&e, &v).unwrap();
        let secret = resolved.masked("TOKEN").unwrap();
        assert_eq!(secret.real, "real-value-1234");
        assert_eq!(secret.surrogate.len(), "real-value-1234".len());
        assert_ne!(secret.surrogate, secret.real);
        // The guest sees the surrogate, never the real value.
        assert_eq!(
            resolved.guest_value("TOKEN"),
            Some(secret.surrogate.as_str())
        );
        let entries: Vec<_> = resolved.guest_entries().collect();
        assert_eq!(entries, vec![("TOKEN", secret.surrogate.as_str())]);
        assert_eq!(resolved.masked_count(), 1);
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn missing_host_variable_errors_with_key_prefix() {
        let v = vault(&[]);
        let e = env(&[("TOKEN", "${NOPE}", true)]);
        let Err(err) = SandboxEnv::resolve(&e, &v) else {
            panic!("expected an error for an undefined host variable");
        };
        let err = err.to_string();
        assert!(err.starts_with("env.TOKEN:"), "got: {err}");
    }

    #[test]
    fn check_injectable_accepts_a_normal_masked_secret() {
        let v = vault(&[]);
        let e = env(&[("TOKEN", "sk-real-token-0123456789", true)]);
        let resolved = SandboxEnv::resolve(&e, &v).unwrap();
        resolved.check_injectable("TOKEN").unwrap();
    }

    #[test]
    fn check_injectable_rejects_unmasked_or_missing() {
        let v = vault(&[]);
        let e = env(&[("PLAIN", "sk-real-token-0123456789", false)]);
        let resolved = SandboxEnv::resolve(&e, &v).unwrap();
        let err = resolved.check_injectable("PLAIN").unwrap_err().to_string();
        assert!(err.starts_with("env.PLAIN:"), "got: {err}");
        assert!(err.contains("mask = true"), "got: {err}");
        let err = resolved.check_injectable("NOPE").unwrap_err().to_string();
        assert!(err.starts_with("env.NOPE:"), "got: {err}");
    }

    #[test]
    fn check_injectable_rejects_short_values() {
        let v = vault(&[]);
        let e = env(&[("TOKEN", "short", true)]);
        let resolved = SandboxEnv::resolve(&e, &v).unwrap();
        let err = resolved.check_injectable("TOKEN").unwrap_err().to_string();
        assert!(err.contains("shorter than"), "got: {err}");
        assert!(!err.contains("short\""), "value leaked: {err}");
    }

    #[test]
    fn non_ascii_value_masks_by_byte_length_and_is_injectable() {
        // The surrogate matches the byte length, so it has more characters
        // than the real value. Non-ASCII bytes are legal header bytes, so
        // the value stays injectable.
        let real = "🔑-secret-token";
        let v = vault(&[]);
        let e = env(&[("TOKEN", real, true)]);
        let resolved = SandboxEnv::resolve(&e, &v).unwrap();
        let secret = resolved.masked("TOKEN").unwrap();
        assert_eq!(secret.surrogate.len(), real.len());
        assert!(secret.surrogate.chars().count() > real.chars().count());
        assert!(secret.surrogate.is_ascii());
        resolved.check_injectable("TOKEN").unwrap();
    }

    #[test]
    fn check_injectable_rejects_invalid_header_bytes() {
        let v = vault(&[("T", "sk-real-token-0123456789\n")]);
        let e = env(&[("TOKEN", "${T}", true)]);
        let resolved = SandboxEnv::resolve(&e, &v).unwrap();
        let err = resolved.check_injectable("TOKEN").unwrap_err().to_string();
        assert!(err.contains("HTTP header"), "got: {err}");
        assert!(!err.contains("sk-real"), "value leaked: {err}");
    }

    #[test]
    fn debug_never_prints_values() {
        let s = MaskedSecret {
            name: "TOKEN".into(),
            real: "real-secret-value".into(),
            surrogate: "surrogate-value-x".into(),
        };
        let dbg = format!("{s:?}");
        assert!(dbg.contains("TOKEN"));
        assert!(!dbg.contains("real-secret-value"));
        assert!(!dbg.contains("surrogate-value-x"));
    }
}
