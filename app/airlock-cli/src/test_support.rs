//! Shared unit-test helpers: a local-set runtime, a plain executor for
//! async config code, the resolved config of one project file, a process
//! context, fake HTTP servers and self-removing temp dirs.

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use tokio::net::TcpListener;
use tokio::task::LocalSet;

/// Run `fut` to completion on a fresh current-thread runtime inside a
/// `LocalSet`, like `main` does, so `spawn_local` and Cap'n Proto work.
pub fn block_on_local(fut: impl Future<Output = ()>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(fut));
}

/// Run `fut` to completion on the current thread (no runtime: for async
/// code that does no I/O, such as [`crate::config::LayeredConfig::resolve`]).
pub fn block_on<F: Future>(fut: F) -> F::Output {
    futures::executor::block_on(fut)
}

/// The config of one project file `airlock.toml` with `toml`, resolved
/// against the built-in packs and the test pack `sample@1` (see
/// [`crate::packs::init_with_sample`]).
pub fn resolve_project_toml(toml: &str) -> anyhow::Result<crate::config::ResolvedConfig> {
    let layers = crate::config::LayeredConfig::from_values(
        vec![],
        None,
        vec![("airlock.toml", toml::from_str(toml)?)],
    )?;
    block_on(layers.resolve(
        &crate::packs::init_with_sample(),
        &crate::config::ConfigOverrides::default(),
    ))
}

/// `pack` configured with every combination of its arg values (both
/// values of a bool arg, each listed value of a choice arg).
pub fn configured_variants(pack: &crate::packs::Pack) -> Vec<crate::packs::ConfiguredPack> {
    use crate::packs::{ArgKind, ArgValue};
    let mut variants = vec![std::collections::BTreeMap::<String, ArgValue>::new()];
    for arg in pack.args() {
        let choices: Vec<ArgValue> = match &arg.kind {
            ArgKind::Bool => vec![ArgValue::Bool(true), ArgValue::Bool(false)],
            ArgKind::Choice { values, .. } => values.iter().cloned().map(ArgValue::Text).collect(),
        };
        variants = variants
            .into_iter()
            .flat_map(|values| {
                let key = &arg.key;
                choices.iter().map(move |choice| {
                    let mut values = values.clone();
                    values.insert(key.clone(), choice.clone());
                    values
                })
            })
            .collect();
    }
    variants.iter().map(|args| pack.configure(args)).collect()
}

/// The process context of a test: default settings, `vault`, and the
/// database in `home` (a temp dir the caller keeps).
pub fn test_context(home: &Path, vault: crate::vault::Vault) -> crate::context::Context {
    crate::context::Context {
        settings: crate::settings::Settings::load_from(home).unwrap(),
        vault,
        db: crate::db::Db::open(&home.join(crate::db::DIR)).unwrap(),
    }
}

/// Serve `app` on an ephemeral `127.0.0.1` port until the runtime ends.
pub async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Start a test HTTP server that can be shut down on demand.
/// Returns the address and a oneshot sender; dropping the sender stops the server.
pub async fn serve_with_shutdown(app: Router) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, tx)
}

/// A fresh directory under the system temp dir, removed on drop (no
/// `tempfile` dep in this crate).
pub struct TempDir(PathBuf);

impl TempDir {
    /// Create an empty directory whose name contains `tag`, the process id
    /// and a per-process counter.
    pub fn new(tag: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "airlock-test-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
