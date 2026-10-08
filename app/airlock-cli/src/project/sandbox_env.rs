//! Sandbox environment from the `[env]` section.
//!
//! Resolves the environment variables that the guest sees. For an entry with
//! `mask = true`, the guest gets only a surrogate value. The real value stays
//! on the host. The network proxy puts the real value back into outgoing
//! HTTP requests for rules that inject it.

use std::collections::BTreeMap;
use std::fmt;

use rand::rngs::ChaCha20Rng;
use rand::{Rng, SeedableRng};
use sha2::{Digest, Sha256};

use crate::config::config_values::EnvVar;
use crate::vault::Vault;

/// Minimum length of a value that a network rule can inject. A shorter
/// surrogate (random alphanumerics) could appear in unrelated header text,
/// and the byte-level rewrite would then corrupt that text.
pub const MIN_INJECT_LEN: usize = 8;

/// A problem with one `[env]` entry. It has its own type, so `airlock
/// start` can identify it as a configuration error (exit code 2), also when
/// it comes from deep inside the project setup.
#[derive(Debug, thiserror::Error)]
#[error("env.{name}: {reason}")]
pub struct EnvError {
    /// Name of the `[env]` entry.
    pub name: String,
    /// Description of the problem.
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

/// A masked `[env]` entry: the real value (only on the host) and the
/// surrogate that the guest sees instead.
#[derive(Clone)]
pub struct MaskedSecret {
    /// Variable name.
    pub name: String,
    /// Real value. Never sent to the guest.
    pub real: String,
    /// Value that the guest sees, see [`MaskedSecret::new`].
    pub surrogate: String,
}

impl MaskedSecret {
    /// Make a masked entry and its surrogate.
    /// Args:
    ///  - `name`: Variable name
    ///  - `real`: Real value
    ///
    /// Returns:
    ///   Masked entry. The surrogate is an ASCII alphanumeric string with
    ///   the same byte length as `real`. It is stable: it comes from the
    ///   name and the length only, never from the value. Thus tools that
    ///   store the credential on first run (Codex) continue to work after
    ///   restarts and secret rotation.
    pub fn new(name: &str, real: String) -> Self {
        Self {
            name: name.to_string(),
            surrogate: surrogate_for(name, &real),
            real,
        }
    }
}

// Manual Debug, so a stray `{:?}` on a target or connection never prints the
// real value. It also hides the surrogate: when the proxy replaces it, the
// surrogate is as good as the real value.
impl fmt::Debug for MaskedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MaskedSecret")
            .field("name", &self.name)
            .field("len", &self.real.len())
            .finish_non_exhaustive()
    }
}

/// Resolved sandbox environment.
///
/// No `Debug`: the guest values contain substituted secrets for unmasked
/// entries, and the masked map holds the real values.
#[derive(Clone)]
pub struct SandboxEnv {
    /// All `[env]` entries in name order, with the value that the guest
    /// sees.
    guest: Vec<(String, String)>,
    /// Masked entries, by variable name.
    masked: BTreeMap<String, MaskedSecret>,
}

impl SandboxEnv {
    /// Resolve the `[env]` section.
    /// Args:
    ///  - `env`: `[env]` entries from the config
    ///  - `vault`: Vault for `${NAME}` substitution (see [`Vault::subst`])
    ///
    /// Returns:
    ///   Environment with substituted values and surrogates for masked
    ///   entries. [`EnvError`] if a template uses an undefined host
    ///   variable or vault secret.
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

    /// Make an environment with no entries. The read-only project loader
    /// uses it, because it must never resolve secrets.
    pub fn empty() -> Self {
        Self {
            guest: Vec::new(),
            masked: BTreeMap::new(),
        }
    }

    /// Make an env with only the masked secrets `secrets`. The network
    /// test harness uses it.
    #[cfg(test)]
    pub fn from_secrets(secrets: Vec<MaskedSecret>) -> Self {
        let guest = secrets
            .iter()
            .map(|s| (s.name.clone(), s.surrogate.clone()))
            .collect();
        let masked = secrets.into_iter().map(|s| (s.name.clone(), s)).collect();
        Self { guest, masked }
    }

    /// Get the value that the guest sees for `name`: the surrogate for a
    /// masked entry, otherwise the substituted value.
    pub fn guest_value(&self, name: &str) -> Option<&str> {
        self.guest
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Iterate the `(KEY, VALUE)` pairs that the guest sees, in name
    /// order.
    pub fn guest_entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.guest.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Get the masked entry for `name`. `None` if it does not exist or is
    /// not masked.
    pub fn masked(&self, name: &str) -> Option<&MaskedSecret> {
        self.masked.get(name)
    }

    /// Make sure that a network rule can inject `name` into HTTP headers.
    /// The entry must be masked, have at least [`MIN_INJECT_LEN`] bytes,
    /// and be a valid header value.
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
        // Without this check, a stray newline from a `.env` loader would
        // pass startup and then break each request with an opaque 502.
        if hyper::header::HeaderValue::from_str(&secret.real).is_err() {
            return Err(EnvError::new(
                name,
                "injected value contains bytes that are not allowed in an HTTP header \
                 (a newline or control character, perhaps a trailing newline)",
            ));
        }
        Ok(())
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.guest.len()
    }

    /// Check if the environment has no entries.
    pub fn is_empty(&self) -> bool {
        self.guest.is_empty()
    }

    /// Number of masked entries.
    pub fn masked_count(&self) -> usize {
        self.masked.len()
    }
}

/// Domain separator of the surrogate hash. Increase the version if the
/// derivation changes. Then cached surrogates become invalid.
const SURROGATE_DOMAIN: &[u8] = b"airlock-surrogate-v1";

/// The surrogate alphabet, 62 symbols.
const SURROGATE_ALPHABET: &[u8; 62] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Make a deterministic `[A-Za-z0-9]` string with the same byte length as
/// `value`. Only `name` and the length give the result. The value is not
/// in the hash, so the surrogate shows only the length.
fn surrogate_for(name: &str, value: &str) -> String {
    // ChaCha20 seeded with a SHA-256 of the inputs. Unlike `StdRng`, its
    // stream does not change across crate versions.
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
    //! Tests for the surrogates of masked env values and the checks of
    //! values that network rules inject.

    use super::*;
    use crate::test_cfg::host_env_vault;

    /// Test that a surrogate has only ASCII letters and digits and the same
    /// byte length as the real value, so that it fits where the value fits.
    ///   1. Make surrogates for long, short, empty and non-ASCII values
    ///   2. Check the byte length and the characters of each
    #[test]
    fn surrogate_is_alphanumeric_with_same_byte_length() {
        for real in ["sk-ant-oat01-abcdefghijklmnop", "x", "", "ääkkönen-token"] {
            let s = surrogate_for("TOKEN", real);
            assert_eq!(s.len(), real.len(), "{real}");
            assert!(s.chars().all(|c| c.is_ascii_alphanumeric()), "{s}");
        }
    }

    /// Test that the surrogate depends only on the name and the value length,
    /// so that it does not show the value.
    ///   1. Check that two values of the same length give the same surrogate
    ///   2. Check that a different name or length gives a different surrogate
    ///   3. Check that the name and length do not mix, for example "TOKEN1"
    ///      and "TOKEN" with the same value
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

    /// Test that the surrogate derivation does not change, because a change
    /// makes the surrogates that a guest already has invalid.
    ///   1. Make surrogates for three known names and values
    ///   2. Check them against the fixed expected strings
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

    /// Test that a network rule cannot inject an env value that is not masked
    /// or not defined, because only masked values have a surrogate.
    ///   1. Resolve an env with one plain value
    ///   2. Check the injection error for the plain name and an unknown name
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

    /// Test that the debug form of a masked secret has the name but not the
    /// real value or the surrogate, so that logs do not show them.
    ///   1. Make a masked secret
    ///   2. Check its debug text
    #[test]
    fn masked_secret_debug_never_prints_values() {
        let secret = MaskedSecret::new("TOKEN", "real-secret-value".into());
        let debug = format!("{secret:?}");
        assert!(debug.contains("TOKEN"));
        assert!(!debug.contains("real-secret-value"));
        assert!(!debug.contains(&secret.surrogate));
    }
}
