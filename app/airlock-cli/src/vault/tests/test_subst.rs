//! Tests for the substitution of `${NAME}` templates from the host env
//! and the vault secrets.

use super::*;
use crate::test_cfg::vault::{MemoryStorage, vault_with};

/// A storage that panics on each use. It proves that the vault does not
/// open.
struct UntouchableStorage;

impl Storage for UntouchableStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        panic!("the vault must not be opened");
    }
    fn store(&self, _: &str) -> anyhow::Result<()> {
        panic!("the vault must not be written");
    }
}

/// A storage that always fails, as a locked keyring does.
struct LockedKeyring;

impl Storage for LockedKeyring {
    fn load(&self) -> anyhow::Result<Option<String>> {
        anyhow::bail!("keyring locked")
    }
    fn store(&self, _: &str) -> anyhow::Result<()> {
        anyhow::bail!("keyring locked")
    }
}

/// Test that a template takes a value from the host env before a vault
/// secret of the same name, and that an unknown name is an error.
///   1. Set `TOKEN` in the host env and in the vault, and add a vault-only
///      secret
///   2. Substitute a template that uses an env var, `TOKEN` and the secret
///   3. Check that `TOKEN` comes from the host env
///   4. Check that an unknown name gives an error
#[test]
fn substitution_takes_host_env_before_vault_secrets() {
    let vault = vault_with(
        MemoryStorage::default(),
        &[("USER", "alice"), ("TOKEN", "from-env")],
    );
    vault.set_secret("TOKEN", "from-vault").unwrap();
    vault.set_secret("DATABASE_URL", "postgres://db").unwrap();

    assert_eq!(
        vault.subst("${USER}:${TOKEN}@${DATABASE_URL}/x").unwrap(),
        "alice:from-env@postgres://db/x"
    );
    assert!(vault.subst("${NOPE}").is_err());
}

/// Test that a template opens the vault only when a name is not in the host
/// env, so that a plain value does not cause a keyring prompt.
///   1. Use a storage that panics when the vault opens
///   2. Substitute templates without variables and with a host env variable
///   3. Check the results
#[test]
fn vault_is_not_opened_until_template_needs_it() {
    let vault = vault_with(UntouchableStorage, &[("HOME_DIR", "/home/alice")]);

    assert_eq!(vault.subst("plain-value").unwrap(), "plain-value");
    assert_eq!(vault.subst("").unwrap(), "");
    assert_eq!(vault.subst("${HOME_DIR}/x").unwrap(), "/home/alice/x");
}

/// Test that a missing name tells why the vault did not open, not only
/// that the name is missing.
///   1. Use a storage that fails as a locked keyring
///   2. Check that a host env variable still substitutes
///   3. Check that a vault name gives an error with the keyring message
#[test]
fn substitution_reports_why_vault_did_not_open() {
    let vault = vault_with(LockedKeyring, &[("HOST", "from-env")]);

    assert_eq!(vault.subst("${HOST}").unwrap(), "from-env");
    let err = vault.subst("${TOKEN}").unwrap_err();
    assert!(format!("{err:#}").contains("keyring locked"), "{err:#}");
}
