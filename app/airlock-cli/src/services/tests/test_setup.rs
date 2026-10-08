use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::services::{ServiceId, build_enabled, unavailable_warning};
use crate::test_cfg::{temp_dir, test_context};
use crate::vault::{Storage, Vault, VaultStorageType};

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
fn enabled_services_with_working_vault_run_with_their_sign_ins() {
    let home = temp_dir();
    let context = test_context(home.path(), vault(MemoryStorage::default()));
    let services = build_enabled(&both(), &context, &tls());
    assert_eq!(services.interceptors().len(), 2);
    assert_eq!(services.browser_grants().len(), 2);
    assert!(services.denied_targets().is_empty());
}

#[test]
fn no_enabled_service_never_opens_vault() {
    let home = temp_dir();
    let context = test_context(home.path(), vault(BrokenStorage));
    let off = BTreeMap::from([("anthropic".into(), false)]);
    let services = build_enabled(&off, &context, &tls());
    assert!(services.interceptors().is_empty());
    assert!(services.denied_targets().is_empty());
}

#[test]
fn enabled_services_without_vault_are_unavailable_and_their_hosts_denied() {
    for (vault, reason) in [
        (
            Vault::for_storage_type(VaultStorageType::Disabled),
            "the vault is disabled",
        ),
        (vault(BrokenStorage), "the keyring is locked"),
    ] {
        let home = temp_dir();
        let context = test_context(home.path(), vault);
        let services = build_enabled(&both(), &context, &tls());
        assert!(services.interceptors().is_empty());
        assert!(services.browser_grants().is_empty());
        let ids: Vec<ServiceId> = services.unavailable.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, ServiceId::ALL);
        assert!(
            services.unavailable.iter().all(|(_, r)| r.contains(reason)),
            "{:?}",
            services.unavailable
        );
        let hosts = |targets: Vec<crate::network::target::NetworkTarget>| {
            targets
                .into_iter()
                .map(|t| (t.host, t.port))
                .collect::<Vec<_>>()
        };
        let mut want = ServiceId::Anthropic.targets();
        want.extend(ServiceId::Openai.targets());
        assert_eq!(hosts(services.denied_targets()), hosts(want));
    }
}

#[test]
fn unavailable_warning_names_services_reason_and_opt_out() {
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
