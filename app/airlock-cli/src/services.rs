//! Network services: airlock-managed sign-ins of AI agents.
//!
//! A service (`[network.services] anthropic = true`) owns the hosts of one
//! provider's sign-in and API. The agent signs in inside the sandbox as
//! usual (`claude /login`, `codex login`); the browser opens on the host
//! through the browser bridge ([`crate::rpc::browser`]). The proxy keeps
//! the real tokens on the host and the sandbox only ever sees
//! *surrogates*: random strings in the provider's token format. The
//! sandbox is untrusted: it never holds a real credential (not even the
//! authorization code of a sign-in, see [`auth_codes`]), and it cannot
//! make the proxy use one for other requests than the provider's API.
//!
//! ## Shared sign-ins
//!
//! The sign-ins (*grants*) are the user's, not a sandbox's: every airlock
//! process uses the same token store. The packs keep the agents'
//! credential files (with the surrogates) on shared mounts in their pack
//! directories (`~/.cache/airlock/packs/mounts/claude/claude` →
//! `~/.claude/.credentials.json`, `~/.cache/airlock/packs/mounts/codex/codex`
//! → `~/.codex/auth.json`, see [`crate::cache::pack_mounts_dir`]), so the
//! sandboxes act the same as several instances of an agent on one host:
//! they read the same surrogates, a sign-in or refresh rewrites the file
//! for all, and a sign-out (`claude /logout`, and `codex login`, which
//! signs out before it signs in) deletes the grant and revokes it upstream
//! for all. A new sign-in replaces the older grant of the same account
//! and scopes. With the mounts changed to per-project directories each
//! project has its own credential file; the grants are still in the one
//! store.
//!
//! Hooks into the proxy:
//!
//! - [`crate::network::Network::resolve_target`]: an owned host is allowed
//!   (unless `deny-always` or a deny rule), always intercepted, never
//!   passthrough. The resolved target carries the service's
//!   [`crate::network::interceptor::Interceptor`]. Host names match
//!   without regard to case or a trailing dot, and the service's
//!   endpoints are canonical ([`crate::network::target::Endpoint`]).
//! - [`crate::network::http`]: the relay hands each request on an owned
//!   host to [`crate::network::interceptor::Interceptor::send`] around the
//!   upstream send, after the monitor event and the Lua middleware, so
//!   both see surrogates only. The interceptor first pins the request's
//!   authority to the endpoint ([`oauth::pin_authority`]), handles the
//!   token hosts' routes itself (code exchange, refresh, revoke; other
//!   routes there get a local `403`), swaps a known surrogate for its real
//!   value in the credential headers (`Authorization: Bearer`,
//!   `x-api-key`) on the API hosts (any path; never another header, never
//!   a body, never another host), scans every API answer for real tokens
//!   as it streams ([`scan`]), and puts surrogates in place of the real
//!   tokens in token answers ([`tokens`]). Only TLS connections get
//!   the interceptor: on plain HTTP a surrogate goes out as it is.
//! - The browser bridge ([`crate::rpc::browser`]): each service's
//!   [`sign_in::LoopbackSignIn`] is a browser grant for its sign-in
//!   pages, and `airlock start` points `$BROWSER` of the sandbox at the
//!   guest's browser shim. Once the VM is booted ([`Services::attach`]),
//!   the grant forwards the page's callback port into the guest
//!   ([`callback`]), which swaps the authorization code for a surrogate
//!   code ([`auth_codes`]), and keeps the page's PKCE challenge for
//!   Claude's manual sign-in exchange. The check of a page takes any
//!   OAuth client and any well-formed scopes ([`sign_in`]): the exchange
//!   stores the client it used, and refresh and revoke use that one.
//!
//! ## Fail closed
//!
//! - Dispatch: a token host serves only the routes the agents call there
//!   (token, revoke, and the few sign-in calls of each agent); any other
//!   route gets a local `403`. An API host takes every path, with the
//!   credential swap and the answer scan. A host the service does not know passes the backstop of
//!   [`oauth`]; nothing goes out as a raw forward.
//! - Unknown token formats: a token answer with a string under a
//!   token-like key that is in no format the provider's table knows is
//!   refused whole (local `502`, nothing stored), so a new token format
//!   never reaches the sandbox ([`tokens`]).
//! - Strict credentials: on the API branch, an `Authorization` or
//!   `x-api-key` value must be a surrogate of the service or the real
//!   value of a masked secret the inject rules put in (the `injected` of
//!   [`crate::network::interceptor::Interceptor::send`]); anything else
//!   gets a local `401` that names the two ways. One sandbox so cannot
//!   plant its own token in the shared credential files and have the
//!   others' requests use it.
//! - No token store: an enabled service without its store key (vault
//!   `disabled`, or a key that fails) is unavailable, and its hosts
//!   are denied under every policy ([`build_enabled`]), so the agent cannot sign
//!   in natively and keep real tokens in the sandbox. `airlock show` says
//!   so; `[network.services] <name> = false` is the opt-out.
//!
//! The agent owns the token lifecycle, as it would on a host: it
//! refreshes its own real access token, and the proxy only relays that
//! call and stores the answer (see [`oauth::Grants::relay_refresh`]). An
//! API request past its real expiry gets whatever the provider answers
//! (a 401 included); the proxy never refreshes on its own. Real tokens
//! live encrypted in a database shared by all airlock processes
//! ([`store`]); its key is in the vault. A sign-out or refresh in one
//! process is seen by the others on their next lookup. A sign-out revokes both real
//! tokens where the provider allows it, a replaced grant is revoked too,
//! and the tokens of a refresh that ends after a sign-out are revoked
//! again (see [`oauth::Grants`]).
//!
//! Providers: [`anthropic`] (Claude Code), [`openai`] (Codex). Shared
//! OAuth 2 logic is in [`oauth`], the surrogate engine in [`tokens`].
//!
//! What stays per provider (and why): the hosts and token-host routes
//! (the allowlist is the point), the token formats (fail closed needs to
//! know a real token), the callback ports and paths of the loopback
//! sign-in (no arbitrary host ports), the device flow and manual sign-in
//! redirects, the shape of the refresh request (Claude Code's drops
//! `org:create_api_key`), and the secret-minting `create_api_key`
//! endpoint (rate limit).

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

/// The services airlock knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServiceId {
    Anthropic,
    Openai,
}

impl ServiceId {
    pub const ALL: [ServiceId; 2] = [ServiceId::Anthropic, ServiceId::Openai];

    /// The name in `[network.services]` and in the token store.
    pub fn name(self) -> &'static str {
        match self {
            ServiceId::Anthropic => "anthropic",
            ServiceId::Openai => "openai",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The hosts the service owns in production.
    pub fn targets(self) -> Vec<NetworkTarget> {
        match self {
            ServiceId::Anthropic => anthropic::Endpoints::production().targets(),
            ServiceId::Openai => openai::Endpoints::production().targets(),
        }
    }
}

/// One provider's sign-in, as the sandbox runs it.
pub trait Service {
    fn id(&self) -> ServiceId;

    /// The service's interceptors over the hosts it owns — each provider
    /// exposes its proxy as a [`crate::network::interceptor::Interceptor`].
    fn interceptors(&self) -> Vec<Rc<dyn Interceptor>>;

    /// The sign-in pages the guest may open on the host.
    fn browser_grants(&self) -> Vec<Rc<dyn BrowserGrant>>;

    /// Reach the booted VM: from now on the sign-ins forward their
    /// callbacks into `guest`.
    fn attach(&self, guest: &GuestNetwork);

    /// Let go of the VM: the callback ports are free once the future
    /// ends.
    fn detach(&self) -> LocalBoxFuture<'_, ()>;
}

/// A provider's [`Service`]: its proxy (an [`Interceptor`]) plus identity
/// and sign-ins. The provider's struct implements only [`Interceptor`]
/// (the actual request handling); this wraps it so [`build_enabled`] can
/// hand out `Rc<dyn Service>` while [`Interceptor`] stays unaware of
/// service identity or sign-in pages.
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

/// The services enabled in `config` (`[network.services]`), in a fixed
/// order. Names were validated with the config.
pub fn enabled(config: &BTreeMap<String, bool>) -> Vec<ServiceId> {
    ServiceId::ALL
        .into_iter()
        .filter(|id| config.get(id.name()).copied().unwrap_or(false))
        .collect()
}

/// The enabled services of a sandbox: those that run, and those that
/// cannot (their hosts are denied).
#[derive(Default)]
pub struct Services {
    running: Vec<Rc<dyn Service>>,
    /// Enabled services without their token store, with the reason.
    unavailable: Vec<(ServiceId, String)>,
}

impl Services {
    /// The interceptors of the running services, for the network.
    pub fn interceptors(&self) -> Vec<Rc<dyn Interceptor>> {
        self.running.iter().flat_map(|s| s.interceptors()).collect()
    }

    /// The hosts the network denies under every policy: those of the
    /// services that cannot run, so the agents never sign in without
    /// airlock.
    pub fn denied_targets(&self) -> Vec<NetworkTarget> {
        self.unavailable
            .iter()
            .flat_map(|(id, _)| id.targets())
            .collect()
    }

    /// The browser grants of the running services.
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

    /// Detach every running service; their ports are free afterwards.
    pub async fn detach(&self) {
        for service in &self.running {
            service.detach().await;
        }
    }
}

/// Build the services enabled in `config`; each has its own surrogate
/// codes (and opened sign-in pages), shared by its token exchange and its
/// sign-ins. Needs the
/// token-store key from the vault of `context` and its database (open
/// since [`Context::load`]). Fails closed: with the vault `disabled`, or a
/// key that fails, the services are unavailable and their hosts are
/// denied (one warning per process names them, the reason and the
/// opt-out), so an agent never signs in natively and keeps real tokens in
/// the sandbox. The vault opens only when a service is enabled.
pub fn build_enabled(
    config: &BTreeMap<String, bool>,
    context: &Context,
    tls_client: &Arc<rustls::ClientConfig>,
) -> Services {
    static WARNED: AtomicBool = AtomicBool::new(false);

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

/// The warning about enabled services that cannot run.
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

/// Print `msg` as a warning, the first time only. Returns whether it was
/// printed.
fn warn_once(warned: &AtomicBool, msg: &str) -> bool {
    if warned.swap(true, Ordering::Relaxed) {
        return false;
    }
    cli::log!("{} {msg}", cli::yellow("warning:"));
    true
}

/// The token store in the database of `context`, with the key from its
/// vault (created there on first use).
fn open_store(context: &Context) -> anyhow::Result<store::TokenStore> {
    if context.vault.storage_type() == VaultStorageType::Disabled {
        anyhow::bail!("the vault is disabled");
    }
    let key = context.vault.service_store_key()?;
    Ok(store::TokenStore::new(context.db.clone(), &key))
}

#[cfg(test)]
mod tests;
