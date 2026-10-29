//! Setup env of a pack.
//!
//! A pack `config.lua` can give values to its setup script. The values
//! can be secrets, so airlock never logs them, never shows them and never
//! stores them. Only the names of the values show. Airlock finds changed
//! values through keyed hashes. The hash key is random and stays private
//! in the data directory.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

use anyhow::Context as _;
use hmac::{Hmac, Mac as _};
use sha2::Sha256;

use crate::data_dir::DataDir;
use crate::messages;
use crate::util::VerifiedPath;

/// Prefix of the setup env variables of the setup script.
pub const ENV_PREFIX: &str = "AIRLOCK_PACK_ENV_";

/// Maximum number of setup env keys of one pack.
pub const MAX_KEYS: usize = 64;

/// Maximum size of one variable `AIRLOCK_PACK_ENV_<KEY>=<value>`, with the
/// end NUL. This is the Linux limit for one exec string (MAX_ARG_STRLEN).
pub const MAX_ENTRY: usize = super::install::compose::MAX_ARG_STRLEN;

/// Maximum size of all setup env variables of one pack, counted as for
/// [`MAX_ENTRY`]. The RPC to the guest allows messages of 64 MiB (the
/// default traversal limit of the message reader). The exec in the guest
/// allows about 2 MiB of args and env in total. The setup script and the
/// sandbox env also use that space.
pub const MAX_TOTAL: usize = 1024 * 1024;

/// Name of the file with the hash key, in the data directory.
pub const KEY_FILE: &str = "pack-env.key";

/// Setup env of a pack: values that `config.lua` gives to `setup.sh`, by
/// key. All keys and values are valid (see [`SetupEnv::new`]).
///
/// The values can be secrets. Thus `Debug` shows only the keys, and the
/// type cannot serialize.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SetupEnv(BTreeMap<String, String>);

impl SetupEnv {
    /// Check and make a setup env.
    /// Args:
    ///  - `vars`: Values by key
    ///
    /// Returns:
    ///   The setup env, or an error text if a key or a value is not valid or
    ///   a limit is exceeded. The text does not contain a value.
    pub fn new(vars: BTreeMap<String, String>) -> Result<Self, String> {
        if vars.len() > MAX_KEYS {
            return Err(messages::packs::setup_env_too_many_keys(MAX_KEYS));
        }
        let mut total = 0;
        for (key, value) in &vars {
            if !valid_key(key) {
                let key = crate::cli::prompt::style::escape_controls(key, false);
                return Err(messages::packs::setup_key_invalid(format!(
                    "setup.env.{key}"
                )));
            }
            if value.contains('\0') {
                // The key is valid here, thus it needs no escape.
                return Err(messages::packs::setup_env_value_not_text(key));
            }
            let size = entry_size(key, value);
            if size > MAX_ENTRY {
                return Err(messages::packs::setup_env_entry_too_large(key, MAX_ENTRY));
            }
            total += size;
        }
        if total > MAX_TOTAL {
            return Err(messages::packs::setup_env_too_large(MAX_TOTAL));
        }
        Ok(Self(vars))
    }

    /// Make a setup env with no check, for tests.
    #[cfg(test)]
    pub fn for_test(vars: &[(&str, &str)]) -> Self {
        Self(
            vars.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    /// Return true if the setup env has no keys.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Get the keys, sorted.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Get the variables for the setup script.
    /// Returns:
    ///   `(AIRLOCK_PACK_ENV_<KEY>, value)` for each key, sorted by key.
    pub fn vars(&self) -> impl Iterator<Item = (String, String)> + '_ {
        self.0
            .iter()
            .map(|(key, value)| (format!("{ENV_PREFIX}{key}"), value.clone()))
    }

    /// Make the keyed hash of all keys and values. It changes when a key or
    /// a value changes.
    /// Args:
    ///  - `key`: Hash key
    ///
    /// Returns:
    ///   HMAC-SHA256 of the sorted pairs. The hash input has the length of
    ///   each key and value before it, thus two different envs never give
    ///   the same input.
    pub fn mac(&self, key: &EnvMacKey) -> [u8; 32] {
        let mut mac = key.hmac();
        for (name, value) in &self.0 {
            update_prefixed(&mut mac, name.as_bytes());
            update_prefixed(&mut mac, value.as_bytes());
        }
        mac.finalize().into_bytes().into()
    }

    /// Make the keyed hash of each value. Airlock compares them to name the
    /// keys whose value changed.
    /// Args:
    ///  - `key`: Hash key
    ///
    /// Returns:
    ///   HMAC-SHA256 of the key and the value (each with its length before
    ///   it), by key.
    pub fn key_macs(&self, key: &EnvMacKey) -> BTreeMap<String, [u8; 32]> {
        self.0
            .iter()
            .map(|(name, value)| {
                let mut mac = key.hmac();
                update_prefixed(&mut mac, name.as_bytes());
                update_prefixed(&mut mac, value.as_bytes());
                (name.clone(), mac.finalize().into_bytes().into())
            })
            .collect()
    }
}

impl fmt::Debug for SetupEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Show the keys only. The values can be secrets.
        f.debug_tuple("SetupEnv")
            .field(&self.0.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Return true if `key` is a valid setup env key: `[A-Z][A-Z0-9_]*`.
fn valid_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_uppercase())
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Get the size of the exec string `AIRLOCK_PACK_ENV_<key>=<value>` with
/// its end NUL.
fn entry_size(key: &str, value: &str) -> usize {
    ENV_PREFIX.len() + key.len() + 1 + value.len() + 1
}

/// Add `bytes` to `mac`, with its length (8 bytes, big endian) before it.
fn update_prefixed(mac: &mut Hmac<Sha256>, bytes: &[u8]) {
    mac.update(&(bytes.len() as u64).to_be_bytes());
    mac.update(bytes);
}

/// Key for the keyed hashes of setup envs. It is random and stays the same
/// for one data directory.
pub struct EnvMacKey([u8; 32]);

impl EnvMacKey {
    /// Read the hash key of the data directory. Make a new random key if
    /// there is none.
    /// Args:
    ///  - `data_dir`: Airlock data directory
    ///
    /// Returns:
    ///   The key, or an error if the key file cannot be read or made, or if
    ///   it is not valid.
    pub fn load_or_create(data_dir: &DataDir) -> anyhow::Result<Self> {
        let file = data_dir.root.join(KEY_FILE)?;
        Self::read(&file)
            .transpose()
            .unwrap_or_else(|| {
                create(&file)?;
                Self::read(&file)?.context(messages::packs::env_key_file_invalid(&file))
            })
            .with_context(|| messages::packs::env_key_file_failed(&file))
    }

    /// Read the key file `file`.
    /// Returns:
    ///   The key, `None` if the file does not exist, or an error if the
    ///   read fails or the file is not valid.
    fn read(file: &VerifiedPath) -> anyhow::Result<Option<Self>> {
        let text = match file.read_to_string_capped(128) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut key = [0u8; 32];
        hex::decode_to_slice(text.trim(), &mut key)
            .ok()
            .with_context(|| messages::packs::env_key_file_invalid(file))?;
        Ok(Some(Self(key)))
    }

    /// Make an HMAC-SHA256 with this key.
    fn hmac(&self) -> Hmac<Sha256> {
        <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(&self.0)
            .expect("HMAC takes keys of any length")
    }
}

impl fmt::Debug for EnvMacKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EnvMacKey(..)")
    }
}

/// Make the key file `file` with a new random key and mode 0600. If an
/// other process made the file first, keep that file.
fn create(file: &VerifiedPath) -> anyhow::Result<()> {
    let key = crate::services::store::random_bytes::<32>()?;
    let dir = file
        .parent()
        .with_context(|| messages::packs::env_key_file_invalid(file))?;
    // Write a full temporary file, then link it to the final name. The link
    // fails if the name exists. Thus a reader never sees a partial file, and
    // two processes agree on one key.
    remove_stale_tmp(&dir);
    let tmp = dir.join(format!(".{KEY_FILE}.{}.tmp", std::process::id()))?;
    let _ = tmp.remove_file();
    let result = (|| -> anyhow::Result<()> {
        let mut out = tmp.open_with(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW),
        )?;
        out.write_all(format!("{}\n", hex::encode(key)).as_bytes())?;
        out.sync_all()?;
        match tmp.hard_link_to(file) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e.into()),
            _ => Ok(()),
        }
    })();
    let _ = tmp.remove_file();
    result
}

/// Remove the temporary key files in `dir` that are older than one minute.
/// A crash between the write and the remove leaves such a file. A live
/// process keeps its file for a very short time only, thus a file of this
/// age is not in use. The removal is best effort.
fn remove_stale_tmp(dir: &VerifiedPath) {
    let Ok(entries) = dir.read_dir() else {
        return;
    };
    let prefix = format!(".{KEY_FILE}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".tmp"))
            .is_none()
        {
            continue;
        }
        let Ok(path) = dir.join(name) else {
            continue;
        };
        let stale = path
            .symlink_metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age.as_secs() > 60));
        if stale {
            let _ = path.remove_file();
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the setup env checks and the hash key file.

    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// The error of `SetupEnv::new` for `vars`.
    fn error(vars: &[(&str, String)]) -> String {
        let vars = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        SetupEnv::new(vars).unwrap_err()
    }

    /// Test the key, value and size checks of a setup env. The limits come
    /// from the exec in the guest, so an env over a limit must fail before
    /// the install boot.
    ///   1. Check that bad keys and a value with NUL are refused
    ///   2. Check that an entry of exactly the limit is accepted and one byte
    ///      more is refused
    ///   3. Check the total size and the key count limits
    ///   4. Check that no error text contains the value
    #[test]
    fn setup_env_with_bad_key_or_value_or_over_limits_is_refused() {
        for key in ["", "lower", "1ABC", "A-B", "A B", "Ä"] {
            let e = error(&[(key, "v".into())]);
            assert!(e.contains("invalid key `setup.env."), "{key}: {e}");
        }
        let e = error(&[("TOKEN", "sec\0ret".into())]);
        assert!(e.contains("NUL") && !e.contains("sec"), "{e}");

        // `AIRLOCK_PACK_ENV_K=` and the end NUL.
        let fixed = ENV_PREFIX.len() + 3;
        let fits = "s".repeat(MAX_ENTRY - fixed);
        assert!(SetupEnv::new([("K".into(), fits.clone())].into()).is_ok());
        let e = error(&[("K", format!("{fits}s"))]);
        assert!(e.contains("131072") && !e.contains("sss"), "{e}");

        let part = "s".repeat(MAX_ENTRY - fixed - 1);
        let keys: Vec<String> = (0..9).map(|i| format!("K{i}")).collect();
        let vars: Vec<(&str, String)> = keys.iter().map(|k| (k.as_str(), part.clone())).collect();
        let e = error(&vars);
        assert!(e.contains("1048576") && !e.contains("sss"), "{e}");

        let keys: Vec<String> = (0..=MAX_KEYS).map(|i| format!("K{i}")).collect();
        let vars: Vec<(&str, String)> = keys.iter().map(|k| (k.as_str(), "v".into())).collect();
        assert!(error(&vars).contains("64"));
    }

    /// Test that the hash key file is made one time with mode 0600 and that
    /// the hashes follow the values. The fingerprints of the installs depend
    /// on a stable key.
    ///   1. Put an old and a new temporary key file in the data directory.
    ///      Load the key and check that only the old temporary file is gone.
    ///      Load the key again and check that the file stays the same.
    ///   2. Check the file mode
    ///   3. Check that the hashes change with a value and with the key
    ///      boundary, and that the debug output has no values
    #[test]
    fn env_key_file_is_made_once_and_hashes_follow_values() {
        let tmp = crate::test_cfg::temp_dir();
        let data = crate::test_cfg::oci::data_dir_at(tmp.path());
        let old = tmp.path().join(format!(".{KEY_FILE}.1.tmp"));
        let new = tmp.path().join(format!(".{KEY_FILE}.2.tmp"));
        std::fs::write(&old, "x").unwrap();
        std::fs::write(&new, "x").unwrap();
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_hours(1);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(hour_ago)
            .unwrap();
        let key = EnvMacKey::load_or_create(&data).unwrap();
        assert!(!old.exists() && new.exists());
        let path = tmp.path().join(KEY_FILE);
        let text = std::fs::read_to_string(&path).unwrap();
        let again = EnvMacKey::load_or_create(&data).unwrap();
        assert_eq!(key.0, again.0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let a = SetupEnv::for_test(&[("AB", "c")]);
        let b = SetupEnv::for_test(&[("A", "Bc")]);
        let c = SetupEnv::for_test(&[("AB", "d")]);
        assert_ne!(a.mac(&key), b.mac(&key));
        assert_ne!(a.mac(&key), c.mac(&key));
        assert_eq!(a.mac(&key), a.clone().mac(&again));
        assert_ne!(a.key_macs(&key)["AB"], c.key_macs(&key)["AB"]);
        assert_eq!(format!("{c:?}"), r#"SetupEnv(["AB"])"#);

        std::fs::write(&path, "bad\n").unwrap();
        let e = format!("{:#}", EnvMacKey::load_or_create(&data).unwrap_err());
        assert!(e.contains("not valid"), "{e}");
    }
}
