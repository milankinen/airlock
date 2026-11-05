//! Sandbox process.
//!
//! The sandbox process owns the VM of one sandbox. A client command starts
//! it and gives it the held sandbox locks. Thus no other process can take
//! the sandbox between the two processes. The sandbox process does the
//! start work on the terminal of its parent, then tells the parent that
//! the sandbox is ready. The start work depends on the mode:
//!  * `airlock start`: all prepare steps with their questions, the
//!    installs and the boot. A failed first start removes the new sandbox
//!  * `airlock exec`: the boot with the config of the last start. Only the
//!    vault can ask a question
//!  * `airlock acp`: as `airlock exec`, but with no questions at all
//!
//! The parent has the first client connection on a socket that it gives at
//! the start. After the parent attached, the sandbox process detaches from
//! the terminal and writes only to the sandbox log.
//!
//! Other clients connect to the sandbox process over the CLI socket. Each
//! connection holds the sandbox. The sandbox stops when the last
//! connection closes, when a client requests a stop, when no connection
//! comes in time, or on SIGTERM. When the parent stops before the detach,
//! the sandbox process stops too.
//!
//! The sandbox process also sends a monitor feed to the clients that
//! attach a monitor.

use std::cell::{Cell, RefCell};
use std::os::unix::io::RawFd;
use std::rc::Rc;

use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;

use super::init::{
    INIT_CAP, Init, MESSAGE_CAP, Mode, Ready, ToChild, ToParent, read_message, write_message,
};
use crate::cli::{self, logging};
use crate::config::ConfigOverrides;
use crate::context::Context;
use crate::oci::OciImage;
use crate::packs::approval::PacksNeedApproval;
use crate::sandbox::first_start::FirstStart;
use crate::sandbox::lock::{HeldLock, SandboxLock};
use crate::sandbox::process::vm::hosted::{HostedOptions, run_hosted};
use crate::sandbox::start::config_change::{self, LastRunInput};
use crate::sandbox::start::{Exit, install, prepare};
use crate::sandbox::{self, Interrupted, Project, registry};
use crate::util::{self, PinnedDir, VerifiedPath};
use crate::vm::{self, VmmBackend};
use crate::{messages, oci, packs};

/// The checked sandbox of the sandbox process.
struct Target {
    /// Canonical project directory.
    project: VerifiedPath,
    /// Registered sandbox directory of the project.
    sandbox_dir: VerifiedPath,
}

/// Run the sandbox process until the sandbox stops (see `airlock internal
/// vmm`).
/// Args:
///  - `init_fd`: Inherited descriptor of the init socket
///  - `context`: Process context
///
/// Returns:
///   0 after a normal stop, 130 if a signal, the parent or the user stops
///   the init, the exit code of `airlock start` for a failed start (for
///   example 2 for a config error), 1 on other errors. An error before the
///   detach goes to the error output.
pub async fn run(init_fd: RawFd, context: Context) -> i32 {
    let (mut reader, writer, watch_stream) = match open_channel(init_fd) {
        Ok(channel) => channel,
        Err(e) => return fail(&e),
    };
    let init = match read_message::<ToChild>(&mut reader, INIT_CAP).await {
        Ok(Some(ToChild::Init(init))) => init,
        Ok(Some(ToChild::Detach) | None) => {
            return fail(&anyhow::anyhow!(messages::sandbox_process::NO_INIT));
        }
        Err(e) => return fail(&e),
    };
    cli::set_silent(init.log.quiet);
    cli::set_verbose(init.log.verbose);
    // An agent client reads the protocol on its terminal. No question can
    // get an answer, so each question fails at once.
    if init.mode == Mode::Acp {
        cli::set_interactive(false);
    }
    if let Err(e) = check_fds(init_fd, &init) {
        return fail(&e);
    }
    // Take the RPC socket first: the VM helper processes must not get it.
    let rpc = match util::fd::take_socket(init.rpc_fd) {
        Ok(rpc) => rpc,
        Err(e) => return fail(&e.into()),
    };
    // Load the VM backend before other work. A backend that the host cannot
    // run stops the start before the questions and the slow steps.
    let vmm = match vm::load_backend(context.settings.vmm) {
        Ok(vmm) => vmm,
        Err(e) => return fail(&e),
    };
    let target = match locate(&init.sandbox_id, &context).await {
        Ok(target) => target,
        Err(e) => return fail(&e),
    };
    let mut lock = match SandboxLock::adopt(
        init.run_lock_fd,
        init.startup_lock_fd,
        target.project.clone(),
        target.sandbox_dir.clone(),
    ) {
        Ok(lock) => lock,
        Err(e) => return fail(&e),
    };
    // Check before the log file exists. Only `airlock start` fills and
    // removes a new sandbox.
    let mut first = match init.mode {
        Mode::Start { .. } => FirstStart::check(&lock),
        Mode::Exec | Mode::Acp => FirstStart::default(),
    };
    // Panics and traces go to the sandbox log from now on. The lines of the
    // sandbox process start with a prefix. A client can write to the same
    // log.
    let prefix = format!("[sandbox {}] ", std::process::id());
    logging::init_file(lock.dir(), logging::LOG_FILE, init.log.log_level, &prefix);
    tracing::info!("airlock version {}", cli::version_string(true));

    // The parent can stop at any time before the detach. Then the init or
    // the sandbox stops, as for a Ctrl+C. The parent sends nothing before
    // the ready message, so the reader has no more data.
    drop(reader);
    let (detach_tx, detach) = oneshot::channel();
    if let Err(e) = spawn_parent_watch(watch_stream, detach_tx, cli::interrupt) {
        return fail(&e.into());
    }
    // The detach frees the startup lock. After a failed init, it stays held
    // until the error is out. Thus the next client does not start while
    // this process still writes to the terminal.
    let startup = Rc::new(RefCell::new(lock.take_startup()));
    let detached = Rc::new(Cell::new(false));
    let handshake = Handshake {
        writer,
        detach,
        startup: startup.clone(),
        detached: detached.clone(),
    };

    let db = context.db.clone();
    let result = Box::pin(serve(
        &init, &target, &lock, &mut first, context, handshake, rpc, vmm,
    ))
    .await;
    let code = match result {
        Ok(code) | Err(Exit::Code(code)) => code,
        Err(Exit::Failed(e)) if e.is::<Interrupted>() => 130,
        Err(Exit::Failed(e)) => fail(&e),
    };
    // A first start that did not get to the detach failed or was aborted.
    // The locks are still held.
    if !detached.get() {
        first.remove(&db, &lock, &init.sandbox_id).await;
    }
    drop(startup);
    // The run lock goes at the process exit, after the VM stopped.
    drop(lock);
    code
}

/// Report `error` on the error output and in the log. Before the detach,
/// the error output is the terminal of the parent.
/// Returns:
///   The exit code 1.
fn fail(error: &anyhow::Error) -> i32 {
    tracing::error!("sandbox process failed: {error:#}");
    cli::error!("{}", messages::app::command_failed(error));
    1
}

/// Check the inherited descriptors of the init socket `init_fd` and of
/// `init`.
/// Returns:
///   Error if a descriptor is a standard stream, or if two of them are the
///   same.
// Two values with the same number would give one open file two owners,
// and a lock or a socket would then close too early.
fn check_fds(init_fd: RawFd, init: &Init) -> anyhow::Result<()> {
    let fds = [init_fd, init.rpc_fd, init.run_lock_fd, init.startup_lock_fd];
    for (i, fd) in fds.iter().enumerate() {
        if *fd <= 2 || fds[..i].contains(fd) {
            anyhow::bail!(messages::sandbox_process::INIT_FDS_INVALID);
        }
    }
    Ok(())
}

/// Take over the init socket `fd`.
/// Returns:
///   The read and the write half of the socket, and a second handle of the
///   socket for [`spawn_parent_watch`].
fn open_channel(
    fd: RawFd,
) -> anyhow::Result<(
    BufReader<OwnedReadHalf>,
    OwnedWriteHalf,
    std::os::unix::net::UnixStream,
)> {
    let stream = util::fd::take_socket(fd)?;
    stream.set_nonblocking(true)?;
    let watch = stream.try_clone()?;
    let (read, write) = tokio::net::UnixStream::from_std(stream)?.into_split();
    Ok((BufReader::new(read), write, watch))
}

/// Watch the parent and the stop signals in a thread of its own until the
/// detach. A question of the start work blocks the main thread, and the
/// watch must also work then.
/// Args:
///  - `stream`: Handle of the init socket. The thread reads the messages
///    after the init data from it
///  - `detach`: Gets a value for the detach message
///  - `parent_gone`: Called if the parent closes its end or sends a wrong
///    message before the detach, or on SIGTERM or SIGHUP. The process stops
///    the init then: a question sees the interrupt and ends
///
/// Returns:
///   Error if the thread does not start.
fn spawn_parent_watch(
    stream: std::os::unix::net::UnixStream,
    detach: oneshot::Sender<()>,
    parent_gone: impl FnOnce() + Send + 'static,
) -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name("parent-watch".into())
        .spawn(move || {
            let gone = runtime.block_on(async move {
                let stream = match tokio::net::UnixStream::from_std(stream) {
                    Ok(stream) => stream,
                    Err(e) => {
                        tracing::warn!("watch the parent: {e}");
                        return true;
                    }
                };
                // The main thread can block in a question, so its signal
                // task does not run. A hangup of the terminal must also stop
                // the init. After the detach, the terminal is gone.
                tokio::select! {
                    gone = watch_parent(BufReader::new(stream), detach) => gone,
                    () = next_signal(SignalKind::terminate()) => true,
                    () = next_signal(SignalKind::hangup()) => true,
                }
            });
            if gone {
                parent_gone();
            }
        })?;
    Ok(())
}

/// Wait for the next signal `kind`. Waits for ever if the signal does not
/// register.
async fn next_signal(kind: SignalKind) {
    match signal(kind) {
        Ok(mut signal) => {
            signal.recv().await;
        }
        Err(e) => {
            tracing::warn!("register signal: {e}");
            std::future::pending::<()>().await;
        }
    }
}

/// Read the messages of the parent after the init data. Sends on `detach`
/// for the detach message.
/// Args:
///  - `reader`: The init socket
///  - `detach`: Gets a value for the detach message
///
/// Returns:
///   True if the parent closed its end or sent a wrong message before the
///   detach. The process stops the init then.
async fn watch_parent(
    mut reader: impl tokio::io::AsyncBufRead + Unpin,
    detach: oneshot::Sender<()>,
) -> bool {
    // The init data holds the start arguments. Thus log only the kind of
    // the message.
    let what = match read_message::<ToChild>(&mut reader, MESSAGE_CAP).await {
        Ok(Some(ToChild::Detach)) => {
            let _ = detach.send(());
            return false;
        }
        Ok(Some(ToChild::Init(_))) => "second init message",
        Ok(None) => "end of stream",
        Err(_) => "bad message",
    };
    tracing::info!("parent stopped before the detach: {what}");
    true
}

/// The ready and detach steps of the init protocol (see [`init`]).
struct Handshake {
    /// Write half of the init socket. Its drop closes the socket.
    writer: OwnedWriteHalf,
    /// Gets a value when the parent sends the detach message.
    detach: oneshot::Receiver<()>,
    /// Held startup lock. It goes at the detach.
    startup: Rc<RefCell<Option<HeldLock>>>,
    /// Set after the detach.
    detached: Rc<Cell<bool>>,
}

impl Handshake {
    /// Tell the parent that the sandbox is ready, and wait for the detach
    /// message. Then free the startup lock, detach from the terminal and
    /// close the init socket.
    /// Returns:
    ///   `Ok` after the detach. Error [`Interrupted`] after a signal or if
    ///   the parent stopped, or error if the ready message does not go out.
    async fn detach(mut self, ready: Ready) -> anyhow::Result<()> {
        write_message(&mut self.writer, &ToParent::Ready(ready)).await?;
        // A Ctrl+C that came with the detach message wins: the user wants
        // the stop.
        tokio::select! {
            biased;
            () = cli::interrupted() => return Err(Interrupted.into()),
            detach = self.detach => detach.map_err(|_| Interrupted)?,
        }
        self.detached.set(true);
        // Other clients wait on the startup lock. They can connect now.
        self.startup.borrow_mut().take();
        // The parent can stop now. Later output must not fail.
        if let Err(e) = util::stdio_to_null() {
            tracing::warn!("detach the output: {e}");
        }
        // The signals of the terminal must not reach the sandbox any more.
        if let Err(e) = util::fd::new_session() {
            tracing::warn!("new session: {e}");
        }
        // A Ctrl+C on the terminal before the new session reached this
        // process, but not the parent after its detach. With no detached
        // message, the parent sees a stop and reports the interrupt.
        if cli::is_interrupted() {
            return Err(Interrupted.into());
        }
        // The parent waits for the end of the stream. It then knows that
        // this process does not use the terminal any more. The message
        // tells it that this process did not stop.
        if let Err(e) = write_message(&mut self.writer, &ToParent::Detached).await {
            tracing::warn!("send the detached message: {e:#}");
        }
        drop(self.writer);
        tracing::info!("sandbox process detached");
        Ok(())
    }
}

/// Find the registered sandbox `sandbox_id` and its project.
/// Returns:
///   The checked target, or error if the id is not valid or not
///   registered.
async fn locate(sandbox_id: &str, context: &Context) -> anyhow::Result<Target> {
    let not_registered = || anyhow::anyhow!(messages::sandbox_process::not_registered(sandbox_id));
    if !registry::is_valid_id(sandbox_id) {
        return Err(not_registered());
    }
    let registered = registry::get(&context.db, sandbox_id)
        .await?
        .ok_or_else(not_registered)?;
    let project = VerifiedPath::new(&registered, &context.roots())?;
    // The registry has the canonical path. A path that changed since the
    // registration is not the project of the sandbox.
    if project.to_entry_path()?.as_os_str() != registered {
        return Err(not_registered());
    }
    let sandbox_dir = context.data.sandboxes.join(sandbox_id)?;
    Ok(Target {
        project,
        sandbox_dir,
    })
}

/// Do the start work for the mode of `init` and run the sandbox (see
/// [`run`]). `vmm` boots the VMs.
/// Returns:
///   The exit code, or the exit for a failed or aborted start.
#[allow(clippy::too_many_arguments)]
async fn serve(
    init: &Init,
    target: &Target,
    lock: &SandboxLock,
    first: &mut FirstStart,
    context: Context,
    handshake: Handshake,
    rpc: std::os::unix::net::UnixStream,
    vmm: &'static dyn VmmBackend,
) -> Result<i32, Exit> {
    let (project, image, main_argv) = match &init.mode {
        Mode::Start { args } => {
            let prepared = Box::pin(prepare::prepare(
                context,
                lock,
                first,
                args,
                init.log.log_level,
                vmm,
            ))
            .await?;
            (prepared.project, prepared.image, Some(prepared.main_argv))
        }
        Mode::Exec | Mode::Acp => {
            let (project, image) = Box::pin(prepare_last_run(target, lock, context)).await?;
            (project, image, None)
        }
    };
    // A signal during the start work stops the process before the boot.
    if cli::is_interrupted() {
        return Err(Exit::ABORTED);
    }
    let options = HostedOptions {
        log_level: init.log.log_level,
        quiet: cli::is_silent(),
        first_client: Some(rpc),
        vmm,
    };
    let ready = Ready { main_argv };
    Ok(
        Box::pin(run_hosted(project, &image, options, async move || {
            handshake.detach(ready).await
        }))
        .await?,
    )
}

/// Prepare the sandbox for `airlock exec` and `airlock acp`: the config of
/// the last `airlock start` with the prepared image and disk. Asks no
/// question about the config. A missing part tells the user to run
/// `airlock start`.
/// Args:
///  - `target`: The checked sandbox
///  - `lock`: The held sandbox locks
///  - `context`: Process context
///
/// Returns:
///   The opened project and the image, or error.
async fn prepare_last_run(
    target: &Target,
    lock: &SandboxLock,
    context: Context,
) -> anyhow::Result<(Project, OciImage)> {
    let sandbox_dir = lock.dir();
    let packs = packs::init(&context.settings.packs, &context.home)?;
    let resolved = config_change::resolve_last_run(&LastRunInput {
        sandbox_dir,
        data_dir: &context.data,
        packs: &packs,
        project_root: &target.project,
        // Only `airlock start --network` changes the network policy.
        overrides: &ConfigOverrides::default(),
    })
    .await
    .map_err(|e| match e.downcast_ref::<PacksNeedApproval>() {
        Some(_) => anyhow::anyhow!(messages::sandbox_process::approval_needed_run_start(&e)),
        None => anyhow::anyhow!(messages::sandbox_process::last_config_invalid_run_start(
            format!("{e:#}")
        )),
    })?
    .ok_or_else(|| anyhow::anyhow!(messages::cli::no_last_run_config(&target.project)))?;

    let image_cfg = &resolved.values.vm.image;
    let image = oci::load_sandbox_image(&context.data, sandbox_dir)
        .filter(|image| {
            image.name == image_cfg.name || image_cfg.pinned_digest() == Some(&image.image_id)
        })
        .ok_or_else(|| anyhow::anyhow!(messages::sandbox_process::IMAGE_MISSING_RUN_START))?;
    if !sandbox::has_disk(sandbox_dir) {
        anyhow::bail!(messages::sandbox_process::DISK_MISSING_RUN_START);
    }
    let pinned = PinnedDir::pin(sandbox_dir.to_path()?)?;
    let pending = install::pending_installs(&pinned, &resolved.packs, &image.image_id);
    if !pending.is_empty() {
        anyhow::bail!(messages::sandbox_process::installs_pending_run_start(
            pending.join(", ")
        ));
    }

    // A signal during the checks stops the process before the slow steps.
    if cli::is_interrupted() {
        return Err(Interrupted.into());
    }
    // The guest cwd of the last start. An empty value is the project dir.
    let guest_cwd = sandbox::read_run_meta(sandbox_dir)
        .guest_cwd
        .filter(|cwd| !cwd.is_empty());
    let project = sandbox::open(lock, resolved.values, guest_cwd, context)?;
    Ok((project, image))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::test_cfg::sandboxes::data_dir_sandbox;
    use crate::test_cfg::{block_on_local, eventually, temp_dir, test_context};
    use crate::vault::{Vault, VaultStorageType};

    /// Test that the sandbox process accepts only a registered sandbox id,
    /// and finds the project of the sandbox in the registry. A wrong id
    /// must not give the sandbox process the data of another sandbox.
    ///   1. Register a sandbox for a project and check its target
    ///   2. Check that an id that is not registered is refused
    ///   3. Check that a value that is not an id is refused
    #[test]
    fn locate_accepts_only_registered_sandbox_id() {
        let home = temp_dir();
        let context = test_context(
            home.path(),
            Vault::for_storage_type(VaultStorageType::Disabled),
        );
        let project = temp_dir();
        let project_path = std::fs::canonicalize(project.path()).unwrap();
        let (id, dir) = data_dir_sandbox(&context, &project_path);

        let target = block_on_local(locate(&id, &context)).unwrap();
        assert_eq!(target.project.to_path().unwrap(), project_path);
        assert_eq!(
            target.sandbox_dir.to_path().unwrap(),
            std::fs::canonicalize(dir).unwrap()
        );
        assert!(block_on_local(locate("zzzzzzzz", &context)).is_err());
        assert!(block_on_local(locate("../escape", &context)).is_err());
    }

    /// Test that the init descriptors must be distinct and must not be the
    /// standard streams. One open file with two owners would close a lock
    /// or a socket too early.
    ///   1. Check that distinct descriptors over 2 are accepted
    ///   2. Check that a repeated descriptor and a standard stream are
    ///      refused
    #[test]
    fn init_fds_that_repeat_or_are_standard_streams_are_refused() {
        let init = |rpc_fd, run_lock_fd, startup_lock_fd| Init {
            sandbox_id: "abc".into(),
            run_lock_fd,
            startup_lock_fd,
            rpc_fd,
            log: super::super::init::LogOpts {
                log_level: crate::cli::LogLevel::Info,
                verbose: false,
                quiet: true,
            },
            mode: Mode::Exec,
        };
        assert!(check_fds(3, &init(4, 5, 6)).is_ok());
        assert!(check_fds(3, &init(3, 5, 6)).is_err());
        assert!(check_fds(3, &init(4, 5, 5)).is_err());
        assert!(check_fds(3, &init(4, 2, 6)).is_err());
        assert!(check_fds(0, &init(4, 5, 6)).is_err());
    }

    /// Test that `exec` and `acp` refuse a sandbox with no config of a last
    /// start, and a last config with a project pack that nobody approved.
    /// Both fail at once with the text that tells the user to run `airlock
    /// start`. These modes must never ask a config question.
    ///   1. Register and lock a sandbox with no last config
    ///   2. Prepare it with the last config and check the error
    ///   3. Save a last config with a new project pack
    ///   4. Prepare it again and check that it fails at once with the
    ///      approval error, and that the `config.lua` did not run
    #[test]
    fn last_run_without_config_or_with_unapproved_pack_tells_to_run_start() {
        let home = temp_dir();
        let context = test_context(
            home.path(),
            Vault::for_storage_type(VaultStorageType::Disabled),
        );
        let project_tmp = temp_dir();
        let project_path = std::fs::canonicalize(project_tmp.path()).unwrap();
        let (id, _) = data_dir_sandbox(&context, &project_path);
        block_on_local(async {
            let target = locate(&id, &context).await.unwrap();
            let lock =
                SandboxLock::acquire_box(&target.project, &context.data.sandboxes, &id).unwrap();

            let err = prepare_last_run(&target, &lock, context.clone())
                .await
                .err()
                .unwrap();
            assert!(err.to_string().contains("run `airlock start`"), "{err:#}");

            let marker = project_path.join("marker");
            crate::test_cfg::packs::write_user_pack(
                &project_path.join("packs"),
                "marker",
                &[
                    (
                        "pack.toml",
                        "label = \"Marker\"\ndescription = \"Marker\"\n",
                    ),
                    (
                        "config.lua",
                        &format!(
                            "assert(io.open({:?}, \"w\")):close()\n",
                            marker.display().to_string()
                        ),
                    ),
                ],
            );
            let toml = "[packs]\n\"./packs/marker\" = {}\n";
            let config = crate::config::LayeredConfig::from_values(
                vec![],
                None,
                vec![("airlock.toml", toml::from_str(toml).unwrap())],
            )
            .unwrap();
            config_change::save(lock.dir(), &config, &sandbox::meta::HostPaths::default()).unwrap();

            let prepared = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                prepare_last_run(&target, &lock, context),
            )
            .await
            .expect("prepare asked a question");
            let err = prepared.err().unwrap().to_string();
            assert!(err.contains("airlock start"), "{err}");
            assert!(err.contains("./packs/marker"), "{err}");
            assert!(!marker.exists());
        });
    }

    /// Test that a parent that closes its end after the ready message stops
    /// the init of the sandbox process, and that the sandbox process does
    /// not detach then. A sandbox must not run on with no parent.
    ///   1. Make a socket pair and watch the parent end in the watch thread
    ///   2. Start the ready step, read the ready message on the parent end
    ///      and close it
    ///   3. Check that the ready step fails with an interrupt and that the
    ///      stop callback ran
    #[test]
    fn parent_close_before_detach_stops_init() {
        block_on_local(async {
            let (parent, child) = util::fd::socket_pair().unwrap();
            child.set_nonblocking(true).unwrap();
            parent.set_nonblocking(true).unwrap();
            let watch = child.try_clone().unwrap();
            let (_, writer) = tokio::net::UnixStream::from_std(child)
                .unwrap()
                .into_split();
            let gone = Arc::new(AtomicBool::new(false));
            let (detach_tx, detach) = oneshot::channel();
            let flag = gone.clone();
            spawn_parent_watch(watch, detach_tx, move || flag.store(true, Ordering::SeqCst))
                .unwrap();
            let handshake = Handshake {
                writer,
                detach,
                startup: Rc::default(),
                detached: Rc::default(),
            };

            let parent = async {
                let stream = tokio::net::UnixStream::from_std(parent).unwrap();
                let mut reader = BufReader::new(stream);
                let ready = read_message::<ToParent>(&mut reader, MESSAGE_CAP)
                    .await
                    .unwrap();
                assert_eq!(ready, Some(ToParent::Ready(Ready::default())));
            };
            let (result, ()) = tokio::join!(handshake.detach(Ready::default()), parent);
            assert!(result.unwrap_err().is::<Interrupted>());
            eventually("stop callback ran", || {
                gone.load(Ordering::SeqCst).then_some(())
            });
        });
    }

    /// Test that the parent watch sees a parent that stops while the main
    /// thread blocks, as in a question of the start work. Else the sandbox
    /// process keeps the question and the locks with no parent.
    ///   1. Watch the child end of a socket pair
    ///   2. Close the parent end and block the main thread with no runtime
    ///   3. Check that the stop callback ran during the block
    #[test]
    fn parent_stop_is_seen_while_main_thread_blocks() {
        let (parent, child) = util::fd::socket_pair().unwrap();
        child.set_nonblocking(true).unwrap();
        let gone = Arc::new(AtomicBool::new(false));
        let (detach_tx, _detach) = oneshot::channel();
        let flag = gone.clone();
        spawn_parent_watch(child, detach_tx, move || flag.store(true, Ordering::SeqCst)).unwrap();

        drop(parent);
        let start = std::time::Instant::now();
        while !gone.load(Ordering::SeqCst) && start.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(gone.load(Ordering::SeqCst));
    }
}
