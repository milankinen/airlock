//! Network services: sign-ins of AI agents that airlock manages.
//!
//! A network service (`[network.services] anthropic = true`) controls the
//! sign-in and API hosts of one provider. The agent signs in inside the
//! sandbox as usual, and the sign-in page opens in the host's browser.
//! The real tokens stay on the host. The sandbox gets only *surrogates*:
//! random strings in the token format of the provider.
//!
//! The sandbox is untrusted. It never holds a real credential. It cannot
//! make the proxy use a real credential for requests other than the
//! requests to the provider's API. When a service gets an unknown request
//! or answer, it refuses it (it fails closed).

pub mod anthropic;
pub mod auth_codes;
pub mod callback;
pub mod oauth;
pub mod openai;
pub mod scan;
pub mod sign_in;
pub mod store;
pub mod tokens;

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::LocalBoxFuture;

use crate::cli;
use crate::context::Context;
use crate::network::interceptor::Interceptor;
use crate::network::target::NetworkTarget;
use crate::rpc::browser::BrowserGrant;
use crate::rpc::guest_network::GuestNetwork;
use crate::services::auth_codes::PendingCodes;
use crate::services::sign_in::LoopbackSignIn;
use crate::vault::VaultStorageType;

/// A network service that airlock knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServiceId {
    /// Claude Code sign-ins and the Anthropic API.
    Anthropic,
    /// Codex sign-ins and the ChatGPT backend.
    Openai,
}

impl ServiceId {
    /// All known services, in a fixed order.
    pub const ALL: [ServiceId; 2] = [ServiceId::Anthropic, ServiceId::Openai];

    /// Name of the service in `[network.services]` and in the token store.
    pub fn name(self) -> &'static str {
        match self {
            ServiceId::Anthropic => "anthropic",
            ServiceId::Openai => "openai",
        }
    }

    /// Find the service with the given `[network.services]` name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Hosts that the service owns in production.
    pub fn targets(self) -> Vec<NetworkTarget> {
        match self {
            ServiceId::Anthropic => anthropic::Endpoints::production().targets(),
            ServiceId::Openai => openai::Endpoints::production().targets(),
        }
    }
}

/// Sign-in and API access of one provider, as a sandbox runs it.
///
/// Sign-ins (*grants*) belong to the user, not to one sandbox. All airlock
/// processes use the same token store. The packs keep the agents'
/// credential files (with the surrogates) on shared mounts (see
/// [`crate::cache::pack_mounts_dir`]). Thus sandboxes act the same as
/// several instances of an agent on one host:
///  * They read the same surrogates.
///  * A sign-in or refresh writes the file again for all of them.
///  * A sign-out deletes the grant and revokes it upstream for all of them.
///    (`codex login` signs out before it signs in.)
///
/// A new sign-in replaces the older grant of the same account and scopes.
/// If the mounts are per project, each project has its own credential
/// file, but the grants are still in the one store.
pub trait Service {
    /// The service identity.
    fn id(&self) -> ServiceId;

    /// The interceptors of the hosts that the service owns.
    ///
    /// The network resolves an owned host as allowed (unless a deny rule
    /// or `deny-always` applies) and always intercepted, never as a
    /// passthrough. Host names match without regard to case or a trailing
    /// dot. The HTTP relay calls the interceptor after the monitor
    /// event and the Lua middleware, so both see only surrogates. Only TLS
    /// connections get the interceptor: on plain HTTP, a surrogate goes
    /// upstream unchanged.
    fn interceptors(&self) -> Vec<Rc<dyn Interceptor>>;

    /// The browser grants of the sign-in pages that the guest can open on
    /// the host.
    fn browser_grants(&self) -> Vec<Rc<dyn BrowserGrant>>;

    /// Connect the sign-ins to the booted VM. After this call, the
    /// sign-ins forward their callbacks into `guest`.
    fn attach(&self, guest: &GuestNetwork);

    /// Disconnect the sign-ins from the VM. The callback ports are free
    /// when the future completes.
    fn detach(&self) -> LocalBoxFuture<'_, ()>;
}

/// [`Service`] of one provider: its [`Interceptor`], identity and
/// sign-ins.
///
/// The provider struct implements only [`Interceptor`] (the request
/// handling). This adapter wraps it, so that [`build_enabled`] can return
/// `Rc<dyn Service>` and [`Interceptor`] does not know about service
/// identity or sign-in pages.
struct ServiceAdapter<T> {
    id: ServiceId,
    interceptor: Rc<T>,
    sign_in: Rc<LoopbackSignIn>,
}

impl<T: Interceptor + 'static> Service for ServiceAdapter<T> {
    fn id(&self) -> ServiceId {
        self.id
    }

    fn interceptors(&self) -> Vec<Rc<dyn Interceptor>> {
        vec![self.interceptor.clone()]
    }

    fn browser_grants(&self) -> Vec<Rc<dyn BrowserGrant>> {
        vec![self.sign_in.clone()]
    }

    fn attach(&self, guest: &GuestNetwork) {
        self.sign_in.attach(guest);
    }

    fn detach(&self) -> LocalBoxFuture<'_, ()> {
        Box::pin(self.sign_in.detach())
    }
}

/// Get the services that `config` (`[network.services]`) enables, in a
/// fixed order. The config validation already checked the names.
pub fn enabled(config: &BTreeMap<String, bool>) -> Vec<ServiceId> {
    ServiceId::ALL
        .into_iter()
        .filter(|id| config.get(id.name()).copied().unwrap_or(false))
        .collect()
}

/// Enabled services of a sandbox: the services that run, and the services
/// that cannot run (their hosts are denied).
#[derive(Default)]
pub struct Services {
    running: Vec<Rc<dyn Service>>,
    /// Enabled services that have no token store, with the reason.
    unavailable: Vec<(ServiceId, String)>,
}

impl Services {
    /// Interceptors of the running services, for the network.
    pub fn interceptors(&self) -> Vec<Rc<dyn Interceptor>> {
        self.running.iter().flat_map(|s| s.interceptors()).collect()
    }

    /// Hosts that the network denies under every policy: the hosts of the
    /// services that cannot run. Thus the agents never sign in without
    /// airlock.
    pub fn denied_targets(&self) -> Vec<NetworkTarget> {
        self.unavailable
            .iter()
            .flat_map(|(id, _)| id.targets())
            .collect()
    }

    /// Browser grants of the running services.
    pub fn browser_grants(&self) -> Vec<Rc<dyn BrowserGrant>> {
        self.running
            .iter()
            .flat_map(|s| s.browser_grants())
            .collect()
    }

    /// Attach every running service to the booted VM's `guest`.
    pub fn attach(&self, guest: &GuestNetwork) {
        for service in &self.running {
            tracing::debug!("service {}: attached", service.id().name());
            service.attach(guest);
        }
    }

    /// Detach every running service. Their ports are free after this call.
    pub async fn detach(&self) {
        for service in &self.running {
            service.detach().await;
        }
    }
}

/// Build the services that `config` enables.
/// Args:
///  - `config`: The `[network.services]` table
///  - `context`: Context with the vault (for the token-store key) and the
///    database (open since [`Context::load`])
///  - `tls_client`: TLS config for the proxy's own calls to token
///    endpoints.
///
/// Returns:
///   The running services, or, if the token store is not available, the
///   unavailable services. Their hosts are then denied.
///
/// If the vault is `disabled` or the key fails, the services are
/// unavailable (fail closed). Thus an agent never signs in natively and
/// keeps real tokens in the sandbox. `[network.services] <name> = false`
/// is the opt-out. The vault opens only when a service is enabled.
pub fn build_enabled(
    config: &BTreeMap<String, bool>,
    context: &Context,
    tls_client: &Arc<rustls::ClientConfig>,
) -> Services {
    static WARNED: AtomicBool = AtomicBool::new(false);

    // One warning per process names the services, the reason and the
    // opt-out.
    let ids = enabled(config);
    if ids.is_empty() {
        return Services::default();
    }
    let store = match open_store(context) {
        Ok(store) => Arc::new(store),
        Err(e) => {
            let reason = format!("{e:#}");
            warn_once(&WARNED, &unavailable_warning(&ids, &reason));
            return Services {
                running: vec![],
                unavailable: ids.into_iter().map(|id| (id, reason.clone())).collect(),
            };
        }
    };
    let running = ids
        .into_iter()
        .map(|id| -> Rc<dyn Service> {
            // Each service has its own surrogate codes (and opened sign-in
            // pages). Its token exchange and its sign-ins share them.
            let codes = PendingCodes::default();
            match id {
                ServiceId::Anthropic => Rc::new(ServiceAdapter {
                    id,
                    interceptor: Rc::new(anthropic::Anthropic::new(
                        anthropic::Endpoints::production(),
                        store.clone(),
                        tls_client.clone(),
                        codes.clone(),
                    )),
                    sign_in: Rc::new(LoopbackSignIn::new(id, anthropic::sign_in_pages(), codes)),
                }),
                ServiceId::Openai => Rc::new(ServiceAdapter {
                    id,
                    interceptor: Rc::new(openai::Openai::new(
                        openai::Endpoints::production(),
                        store.clone(),
                        tls_client.clone(),
                        codes.clone(),
                    )),
                    sign_in: Rc::new(LoopbackSignIn::new(id, openai::sign_in_pages(), codes)),
                }),
            }
        })
        .collect();
    Services {
        running,
        unavailable: vec![],
    }
}

/// Make the warning about enabled services that cannot run.
fn unavailable_warning(ids: &[ServiceId], reason: &str) -> String {
    let names: Vec<&str> = ids.iter().map(|id| id.name()).collect();
    let opt_out: Vec<String> = ids
        .iter()
        .map(|id| format!("`[network.services] {} = false`", id.name()))
        .collect();
    let (services, are, their, agents) = if ids.len() == 1 {
        ("service", "is", "its", "agent")
    } else {
        ("services", "are", "their", "agents")
    };
    format!(
        "the network {services} {} {are} unavailable ({reason}), so airlock denies {their} \
         hosts; to let the {agents} sign in without airlock, set {}",
        names.join(", "),
        opt_out.join(" and ")
    )
}

/// Print `msg` as a warning, only the first time.
/// Returns:
///   `true` if this call printed the warning.
fn warn_once(warned: &AtomicBool, msg: &str) -> bool {
    if warned.swap(true, Ordering::Relaxed) {
        return false;
    }
    cli::log!("{} {msg}", cli::yellow("warning:"));
    true
}

/// Open the token store in the database of `context`, with the key from
/// its vault. The vault creates the key on first use.
fn open_store(context: &Context) -> anyhow::Result<store::TokenStore> {
    if context.vault.storage_type() == VaultStorageType::Disabled {
        anyhow::bail!("the vault is disabled");
    }
    let key = context.vault.service_store_key()?;
    Ok(store::TokenStore::new(context.db.clone(), &key))
}

#[cfg(test)]
mod tests;
