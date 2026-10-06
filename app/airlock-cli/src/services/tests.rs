//! Setup of the services: when they cannot run (their hosts are denied),
//! and the token store in the database of the context.

use std::collections::{BTreeMap, HashMap};

use super::*;
use crate::test_support::test_context;
use crate::vault::{Storage, Vault};

/// A vault backend in memory.
#[derive(Default)]
struct MemoryStorage(parking_lot::Mutex<Option<String>>);

impl Storage for MemoryStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        Ok(self.0.lock().clone())
    }
    fn store(&self, data: &str) -> anyhow::Result<()> {
        *self.0.lock() = Some(data.to_string());
        Ok(())
    }
}

/// A vault backend that cannot be read.
struct BrokenStorage;

impl Storage for BrokenStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        anyhow::bail!("the keyring is locked")
    }
    fn store(&self, _data: &str) -> anyhow::Result<()> {
        anyhow::bail!("the keyring is locked")
    }
}

fn vault(storage: impl Storage) -> Vault {
    Vault::new_with(Box::new(storage), HashMap::new(), VaultStorageType::File)
}

fn both() -> BTreeMap<String, bool> {
    BTreeMap::from([("anthropic".into(), true), ("openai".into(), true)])
}

fn tls() -> Arc<rustls::ClientConfig> {
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth(),
    )
}

#[test]
fn no_enabled_service_needs_no_vault() {
    let off = BTreeMap::from([("anthropic".into(), false)]);
    let home = tempfile::tempdir().unwrap();
    // A vault that fails every read: it is never opened.
    let context = test_context(home.path(), vault(BrokenStorage));
    let services = build_enabled(&off, &context, &tls());
    assert!(services.running.is_empty() && services.unavailable.is_empty());
}

/// Fail closed: with the vault disabled the services do not run, and are
/// unavailable (their hosts denied) rather than off.
#[test]
fn a_disabled_vault_makes_the_services_unavailable() {
    let home = tempfile::tempdir().unwrap();
    let disabled = test_context(
        home.path(),
        Vault::for_storage_type(VaultStorageType::Disabled),
    );
    let services = build_enabled(&both(), &disabled, &tls());
    assert!(services.running.is_empty());
    assert_eq!(
        services.unavailable,
        [
            (ServiceId::Anthropic, "the vault is disabled".to_string()),
            (ServiceId::Openai, "the vault is disabled".to_string()),
        ]
    );
}

/// A vault that fails is no error of `airlock start`: the services are
/// unavailable, with the reason.
#[test]
fn a_failing_vault_makes_the_services_unavailable() {
    let home = tempfile::tempdir().unwrap();
    let context = test_context(home.path(), vault(BrokenStorage));
    let services = build_enabled(&both(), &context, &tls());
    assert!(services.running.is_empty());
    let ids: Vec<ServiceId> = services.unavailable.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, ServiceId::ALL);
    assert!(
        services.unavailable[0].1.contains("the keyring is locked"),
        "{:?}",
        services.unavailable
    );
}

/// The warning names the services, the reason and the opt-out.
#[test]
fn the_unavailable_warning_names_the_opt_out() {
    assert_eq!(
        unavailable_warning(&ServiceId::ALL, "the vault is disabled"),
        "the network services anthropic, openai are unavailable (the vault is disabled), \
         so airlock denies their hosts; to let the agents sign in without airlock, set \
         `[network.services] anthropic = false` and `[network.services] openai = false`"
    );
    assert_eq!(
        unavailable_warning(&[ServiceId::Anthropic], "the vault is disabled"),
        "the network service anthropic is unavailable (the vault is disabled), so airlock \
         denies its hosts; to let the agent sign in without airlock, set \
         `[network.services] anthropic = false`"
    );
}

#[test]
fn a_warning_is_printed_once() {
    let warned = AtomicBool::new(false);
    assert!(warn_once(&warned, "first"));
    assert!(!warn_once(&warned, "second"));
}

/// With a vault, the services run on the database of the context.
#[test]
fn the_services_run_on_the_database_of_the_context() {
    let home = tempfile::tempdir().unwrap();
    let context = test_context(home.path(), vault(MemoryStorage::default()));
    let services = build_enabled(&both(), &context, &tls());
    assert_eq!(services.running.len(), 2);
    assert!(services.unavailable.is_empty());
}
