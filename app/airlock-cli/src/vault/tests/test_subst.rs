use super::*;
use crate::test_cfg::vault::{MemoryStorage, vault_with};

struct UntouchableStorage;

impl Storage for UntouchableStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        panic!("the vault must not be opened");
    }
    fn store(&self, _: &str) -> anyhow::Result<()> {
        panic!("the vault must not be written");
    }
}

struct LockedKeyring;

impl Storage for LockedKeyring {
    fn load(&self) -> anyhow::Result<Option<String>> {
        anyhow::bail!("keyring locked")
    }
    fn store(&self, _: &str) -> anyhow::Result<()> {
        anyhow::bail!("keyring locked")
    }
}

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

#[test]
fn vault_is_not_opened_until_template_needs_it() {
    let vault = vault_with(UntouchableStorage, &[("HOME_DIR", "/home/alice")]);

    assert_eq!(vault.subst("plain-value").unwrap(), "plain-value");
    assert_eq!(vault.subst("").unwrap(), "");
    assert_eq!(vault.subst("${HOME_DIR}/x").unwrap(), "/home/alice/x");
}

#[test]
fn substitution_reports_why_vault_did_not_open() {
    let vault = vault_with(LockedKeyring, &[("HOST", "from-env")]);

    assert_eq!(vault.subst("${HOST}").unwrap(), "from-env");
    let err = vault.subst("${TOKEN}").unwrap_err();
    assert!(format!("{err:#}").contains("keyring locked"), "{err:#}");
}
