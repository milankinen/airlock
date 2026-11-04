//! Project of a sandbox.
//!
//! Loads the project of a sandbox: its paths, its config, the CA of the
//! TLS interception and the resolved environment variables. A locked
//! sandbox gets its CA and records its guest directory at the open. A
//! read-only load needs no lock and resolves no secrets.

use std::fmt::Display;
use std::os::unix::fs::MetadataExt;
use std::time::SystemTime;

use super::env::{EnvError, SandboxEnv};
use super::lock::{SandboxLock, is_running};
use super::lookup::Sandbox;
use super::meta::{last_run_ago, read_run_meta, write_run_meta};
use crate::config::config_values::ConfigValues;
use crate::context::Context;
use crate::messages;
use crate::util::{PinnedDir, VerifiedPath};
use crate::vault::Vault;

/// Size limit of `ca.json`. Defense in depth: only airlock writes the
/// file.
pub(crate) const CA_CAP: u64 = 64 * 1024;

/// A resolved project: its working directory, sandbox paths, config and CA.
///
/// It holds no lock. The caller keeps the [`SandboxLock`] for the run (see
/// [`open`]), and the lock keeps other instances out. Thus `Project` is
/// cheap to [`Clone`]. [`Self::with_config`] uses this for a boot with a
/// different config (the install boot) next to the run config of the
/// original project.
#[derive(Clone)]
pub struct Project {
    /// Sandbox directory: CA, overlay, disk image, lock, run metadata.
    pub sandbox_dir: VerifiedPath,
    /// Host user's home directory.
    pub host_home: VerifiedPath,
    /// Project directory on the host. Its given path is the resolved path.
    pub host_cwd: VerifiedPath,
    /// Working directory inside the container (default: `host_cwd`).
    pub guest_cwd: String,
    /// Resolved project config.
    pub config: ConfigValues,
    /// Resolved `[env]` section: values after host substitution, and
    /// surrogates for masked entries. Only [`open`] and
    /// [`Self::with_config`] set it. The read-only [`Sandbox::load`] never
    /// substitutes secrets, so there it is empty.
    pub env: SandboxEnv,
    /// CA certificate PEM (read from `ca.json` at load time).
    pub ca_cert: String,
    /// CA private key PEM (read from `ca.json` at load time).
    pub ca_key: String,
    /// Process-wide settings, vault and database. The vault opens lazily
    /// (see [`Vault`]), so commands that do not use secrets never show an
    /// unlock prompt.
    pub context: Context,
}

impl Project {
    /// Check if an `airlock start` process runs this project now (see
    /// [`is_running`]).
    pub fn is_running(&self) -> bool {
        is_running(&self.sandbox_dir)
    }

    /// Human-readable time since the last `airlock start` run (for example
    /// "2 hours ago"). `None` if the project never ran.
    pub fn last_run_ago(&self) -> Option<String> {
        last_run_ago(&self.sandbox_dir)
    }

    /// Record a boot (the `last_run` timestamp) in `run.json`. Call it one
    /// time per VM start.
    pub fn save_meta(&self) {
        let mut meta = read_run_meta(&self.sandbox_dir);
        meta.last_run = Some(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
        let _ = write_run_meta(&self.sandbox_dir, &meta);
    }

    /// Working directory for display: the host cwd, and `host → guest` when
    /// the guest cwd is different.
    pub fn display_cwd(&self) -> String {
        let host_cwd = self.host_cwd.to_string();
        if host_cwd == self.guest_cwd {
            host_cwd
        } else {
            format!("{host_cwd} → {}", self.guest_cwd)
        }
    }

    /// Make a clone of this project with a different config.
    /// Args:
    ///  - `config`: Config of the clone. Its `[env]` is resolved again
    ///    (see [`resolve_env`])
    ///
    /// Returns:
    ///   Clone with the same paths, guest cwd and CA. This project does not
    ///   change. Used for the install boot next to the original project
    ///   (see [`crate::sandbox::start::install::install_tools`]).
    pub fn with_config(&self, config: ConfigValues) -> Result<Self, EnvError> {
        let env = resolve_env(&config, &self.context.vault)?;
        Ok(Self {
            config,
            env,
            ..self.clone()
        })
    }
}

impl Sandbox {
    /// Load the project of the sandbox without a lock. Does not make a CA
    /// and does not resolve `[env]`. For read-only subcommands (`info`).
    /// Args:
    ///  - `config`: Resolved project config
    ///  - `context`: Process-wide context from `main`. All projects in one
    ///    process share its vault, so they load secrets only one time.
    ///
    /// Returns:
    ///   The project, or error if the project path fails the path check.
    pub fn load(&self, config: ConfigValues, context: Context) -> anyhow::Result<Project> {
        let host_cwd = self.project()?.clone();
        let sandbox_dir = self.dir.clone();
        let (ca_cert, ca_key) = read_ca(&sandbox_dir).unwrap_or_default();
        let guest_cwd = read_run_meta(&sandbox_dir)
            .guest_cwd
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| host_cwd.to_string());
        Ok(Project {
            sandbox_dir,
            host_home: context.home.clone(),
            host_cwd,
            guest_cwd,
            config,
            env: SandboxEnv::empty(),
            ca_cert,
            ca_key,
            context,
        })
    }
}

/// Prepare the locked sandbox for use. Records the guest cwd, makes the CA
/// keypair if it does not exist, and resolves `[env]` (see
/// [`resolve_env`]).
/// Args:
///  - `lock`: Held sandbox lock. Gives only its paths. The caller must keep
///    the lock while the sandbox must stay exclusive (see [`SandboxLock`])
///  - `config`: Resolved project config
///  - `sandbox_cwd_override`: Working directory inside the container.
///    `None` uses the project directory
///  - `context`: Process-wide context from `main`
///
/// Returns:
///   Project without a lock. Thus a second boot from the same `lock` (see
///   [`Project::with_config`]) needs no lock of its own.
pub fn open(
    lock: &SandboxLock,
    config: ConfigValues,
    sandbox_cwd_override: Option<String>,
    context: Context,
) -> anyhow::Result<Project> {
    let host_cwd = lock.host_cwd().clone();
    let sandbox_dir = lock.dir().clone();
    let guest_cwd = sandbox_cwd_override.unwrap_or_else(|| host_cwd.to_string());

    // Store guest_cwd in run.json, so `airlock exec` can use it as default.
    let mut meta = read_run_meta(&sandbox_dir);
    meta.guest_cwd = Some(guest_cwd.clone());
    write_run_meta(&sandbox_dir, &meta)?;

    if !has_ca(&sandbox_dir) {
        generate_ca(&sandbox_dir)?;
    }
    let (ca_cert, ca_key) = read_ca(&sandbox_dir)?;

    // Resolve `[env]` now, under the lock but before slow steps. A missing
    // host variable or vault secret then fails here, not after an image
    // pull. Also, the network layer needs the masked secrets.
    let env = resolve_env(&config, &context.vault)?;

    Ok(Project {
        sandbox_dir,
        host_home: context.home.clone(),
        host_cwd,
        guest_cwd,
        config,
        env,
        ca_cert,
        ca_key,
        context,
    })
}

/// Resolve the `[env]` section of a config, and check each value that an
/// enabled network rule injects.
/// Args:
///  - `config`: Project config
///  - `vault`: Vault for `${NAME}` substitution
///
/// Returns:
///   Resolved env (substituted values and surrogates for masked entries),
///   or [`EnvError`] for a missing variable or a value that a rule cannot
///   inject.
pub fn resolve_env(config: &ConfigValues, vault: &Vault) -> Result<SandboxEnv, EnvError> {
    let env = SandboxEnv::resolve(&config.env, vault)?;
    for rule in config.network.rules.values().filter(|r| r.is_used()) {
        for name in &rule.inject {
            env.check_injectable(name)?;
        }
    }
    Ok(env)
}

/// Check if the sandbox in `sandbox_dir` has its CA keypair (`ca.json`).
/// A symlink there is no CA: the next open replaces it with a new CA.
pub fn has_ca(sandbox_dir: &VerifiedPath) -> bool {
    sandbox_dir
        .join("ca.json")
        .and_then(|path| path.metadata())
        .is_ok_and(|m| m.is_file())
}

/// Make sure that `.airlock/` exists in `host_cwd` and has a `.gitignore`.
/// Returns:
///   Path of the cache dir (`.airlock/`), checked again after the create.
///   Error if `.airlock` is a symlink or another user owns it.
pub fn ensure_cache_dir(host_cwd: &VerifiedPath) -> anyhow::Result<VerifiedPath> {
    // Do not follow symlinks at `.airlock` and `.airlock/.gitignore` (see
    // `PinnedDir`). A symlinked `.airlock` gives the path check error. For
    // a foreign-owned `.airlock`, give a clear message, not the raw `EPERM`
    // of `PinnedDir::open`.
    let airlock = host_cwd.join(".airlock")?;
    airlock.to_path()?;
    if let Ok(meta) = airlock.symlink_metadata()
        && let Some(problem) = airlock_dir_problem_for(&airlock, meta.uid())
    {
        anyhow::bail!(problem);
    }
    let cache_dir = PinnedDir::open(host_cwd.to_path()?, ".airlock".as_ref(), true)?;
    if cache_dir.ino(".gitignore").is_none() {
        cache_dir.write_atomic(".gitignore", b"*\n", 0o644)?;
    }
    Ok(airlock.reverify()?)
}

/// Message when another user owns `path` (`.airlock`). The raw `EPERM`
/// error of `PinnedDir::open` is not clear.
/// Args:
///  - `path`: The `.airlock` directory
///  - `owner_uid`: User ID of the owner of `path`
///
/// Returns:
///   `None` if the current user owns `path`.
pub(super) fn airlock_dir_problem_for(path: impl Display, owner_uid: u32) -> Option<String> {
    if owner_uid != unsafe { libc::geteuid() } {
        return Some(messages::project::airlock_dir_foreign_owner(path));
    }
    None
}

/// CA keypair data stored in `ca.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct CaData {
    cert: String,
    key: String,
}

/// Generate a self-signed CA keypair and write it to `ca.json`.
fn generate_ca(sandbox_dir: &VerifiedPath) -> anyhow::Result<()> {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

    let mut params = CertificateParams::new(vec![])?;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "airlock CA");

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    let ca_data = CaData {
        cert: cert.pem(),
        key: key_pair.serialize_pem(),
    };
    // ca.json holds the CA private key. Write it owner-only (0600) and
    // atomically. A truncated file after a crash blocks all later runs:
    // `has_ca` skips a new CA, but `read_ca` cannot parse the file.
    let json = serde_json::to_string_pretty(&ca_data)?;
    crate::vault::atomic_write_in(sandbox_dir, "ca.json", json.as_bytes())?;

    Ok(())
}

/// Check that `bytes` is a valid `ca.json`: a JSON object with a CA cert
/// and its private key, both PEM data that the TLS interception can use.
pub(crate) fn is_valid_ca(bytes: &[u8]) -> bool {
    let Ok(ca) = serde_json::from_slice::<CaData>(bytes) else {
        return false;
    };
    rcgen::KeyPair::from_pem(&ca.key)
        .and_then(|key| rcgen::Issuer::from_ca_cert_pem(&ca.cert, key))
        .is_ok()
}

/// Read the CA cert and key PEM strings from `ca.json`.
fn read_ca(sandbox_dir: &VerifiedPath) -> anyhow::Result<(String, String)> {
    let json = sandbox_dir
        .join("ca.json")
        .and_then(|path| path.read_to_string_capped(CA_CAP))
        .map_err(|_| anyhow::anyhow!(messages::project::CA_NOT_FOUND))?;
    let ca: CaData = serde_json::from_str(&json)?;
    Ok((ca.cert, ca.key))
}
