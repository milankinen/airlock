//! Cloud Hypervisor VM backend.
//!
//! Starts and stops the sandbox VM and its file sharing on Linux, and opens
//! connections from the host to the guest. The `airlock` binary contains the
//! hypervisor and the file sharing daemon. The backend extracts them before
//! the boot.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::OwnedFd;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, error, warn};

use super::landlock::{self, Access, Ruleset};
use crate::messages;
use crate::settings::Vmm;
use crate::util::VerifiedPath;
use crate::vm::backend::{HandleFuture, IdMap, VmHandle, VmStopped, VmmBackend};
use crate::vm::config::{VmConfig, VmShare};
use crate::vm::remove_old;

/// VM backend that uses cloud-hypervisor and virtiofsd.
pub struct CloudHypervisor;

impl VmmBackend for CloudHypervisor {
    fn kind(&self) -> Vmm {
        Vmm::CloudHypervisor
    }

    fn kernel_console(&self) -> &'static str {
        "console=hvc0 console=ttyS0"
    }

    // virtiofsd shows the host owner as is. Map it to root in the image
    // layers and to the container user in the other shares.
    fn id_map(&self, container_uid: u32, container_gid: u32) -> Option<(IdMap, IdMap)> {
        // SAFETY: `getuid` and `getgid` cannot fail.
        let (host_uid, host_gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let map = |guest_uid, guest_gid| IdMap {
            host_uid,
            host_gid,
            guest_uid,
            guest_gid,
        };
        Some((map(0, 0), map(container_uid, container_gid)))
    }

    fn boot_vm<'a>(
        &'a self,
        config: &'a VmConfig,
    ) -> HandleFuture<'a, anyhow::Result<Box<dyn VmHandle>>> {
        Box::pin(async move {
            let binaries = extract_binaries(&config.asset_dir)?;
            let vm = CloudHypervisorVm::start(config, &binaries)?;
            Ok(Box::new(vm) as Box<dyn VmHandle>)
        })
    }
}

/// Maximum size of each log file of the VM. The guest controls the serial
/// output and can make errors that cloud-hypervisor logs. The limit keeps
/// the guest from filling the host disk.
const LOG_LIMIT: u64 = 16 * 1024 * 1024;

/// Maximum time for the vsock `CONNECT` handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum length of the handshake answer line.
const HANDSHAKE_MAX_LINE: usize = 64;

/// Number of bytes from the end of the cloud-hypervisor log that go into
/// the error when the VM stops without a request.
const LOG_TAIL_BYTES: u64 = 2048;

/// System paths that virtiofsd reads: its libraries, the loader cache and
/// the open file limit of the kernel. Paths that do not exist are skipped.
/// All of `/proc` is not in the list, because it shows the command lines of
/// all host processes. virtiofsd reopens files through `/proc/self/fd`, and
/// for that, Landlock checks only the rule of the file.
const VIRTIOFSD_SYSTEM_PATHS: &[&str] = &[
    "/usr",
    "/lib",
    "/lib32",
    "/lib64",
    "/etc/ld.so.cache",
    "/nix/store",
    "/gnu/store",
    "/proc/sys/fs/nr_open",
];

/// Step and number of steps of the wait for the log thread of
/// cloud-hypervisor (1 s in total).
const LOG_WAIT_STEP: Duration = Duration::from_millis(10);
const LOG_WAIT_STEPS: u32 = 100;

/// Lowest Landlock ABI version for the Landlock of cloud-hypervisor.
const CH_LANDLOCK_MIN_ABI: u32 = 3;

/// Paths of the executables of the backend.
struct Binaries {
    /// `cloud-hypervisor` executable.
    cloud_hypervisor: VerifiedPath,
    /// `virtiofsd` executable.
    virtiofsd: VerifiedPath,
}

/// Extract the bundled `cloud-hypervisor` and `virtiofsd` to the asset
/// directory `dir` if their checksums changed.
/// Returns:
///   The paths of the executables, or error if the extraction fails.
#[cfg(not(test))]
fn extract_binaries(dir: &VerifiedPath) -> anyhow::Result<Binaries> {
    use crate::vm::assets::{AssetFile, extract};

    extract(
        dir,
        &[
            AssetFile {
                name: "cloud-hypervisor",
                data: include_bytes!("../../../../../target/vm/cloud-hypervisor"),
                checksum: env!("AIRLOCK_CHECKSUM_CLOUD_HYPERVISOR"),
                mode: 0o755,
            },
            AssetFile {
                name: "virtiofsd",
                data: include_bytes!("../../../../../target/vm/virtiofsd"),
                checksum: env!("AIRLOCK_CHECKSUM_VIRTIOFSD"),
                mode: 0o755,
            },
        ],
    )?;
    Ok(Binaries {
        cloud_hypervisor: dir.join("cloud-hypervisor")?,
        virtiofsd: dir.join("virtiofsd")?,
    })
}

/// Test stub. Tests do not extract the VM assets, so this always fails.
#[cfg(test)]
fn extract_binaries(_dir: &VerifiedPath) -> anyhow::Result<Binaries> {
    anyhow::bail!("cloud_hypervisor::extract_binaries not supported in tests")
}

/// A running cloud-hypervisor VM with its virtiofsd processes. A drop
/// kills and reaps all processes that started.
struct CloudHypervisorVm {
    /// cloud-hypervisor process. `None` before the start and after the stop.
    ch_child: Option<Child>,
    /// virtiofsd processes with their share tags.
    virtiofsd_children: Vec<(String, Child)>,
    vsock_socket_path: VerifiedPath,
    runtime_dir: VerifiedPath,
    /// Log of cloud-hypervisor.
    ch_log: VerifiedPath,
    /// Thread that copies the error output of cloud-hypervisor to its log.
    ch_log_thread: Option<std::thread::JoinHandle<()>>,
}

/// A process of the VM.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VmProcess {
    /// The cloud-hypervisor process.
    Hypervisor,
    /// The virtiofsd process of the share with this tag.
    Virtiofsd(String),
}

impl std::fmt::Display for VmProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hypervisor => f.write_str("cloud-hypervisor"),
            Self::Virtiofsd(tag) => write!(f, "virtiofsd {tag}"),
        }
    }
}

impl CloudHypervisorVm {
    /// Start one virtiofsd process for each VirtioFS share, then start
    /// cloud-hypervisor with vsock support.
    /// Args:
    ///  - `config`: The VM to boot
    ///  - `binaries`: The executables to start
    ///
    /// Returns:
    ///   The VM, or error if a process does not start. The processes that
    ///   started before the error are stopped.
    fn start(config: &VmConfig, binaries: &Binaries) -> anyhow::Result<Self> {
        let runtime_dir = &config.runtime_dir;
        let vfs_dir = runtime_dir.join("vfs")?;
        vfs_dir.mkdirs()?;
        cleanup_sockets(runtime_dir);
        let landlock_abi = landlock::abi_version().unwrap_or(0);
        if landlock_abi < CH_LANDLOCK_MIN_ABI {
            warn!("Landlock is missing or old: VM helper processes have fewer file limits");
        }

        // From here, a drop of `vm` stops all processes that started, also
        // on the error paths below.
        let mut vm = Self {
            ch_child: None,
            virtiofsd_children: Vec::new(),
            vsock_socket_path: runtime_dir.join("vsock.sock")?,
            runtime_dir: runtime_dir.clone(),
            ch_log: runtime_dir.join("cloud-hypervisor.log")?,
            ch_log_thread: None,
        };

        let mut fs_args: Vec<String> = Vec::new();
        for share in &config.shares {
            // Use only the last part of the tag (`files/rw` -> `rw`). Short
            // names keep the socket paths in the Unix socket limit. The last
            // parts of the tags are unique.
            let sock_name = share.tag.rsplit('/').next().unwrap_or(&share.tag);
            let sock_path = vfs_dir.join(format!("{sock_name}.sock"))?;
            let _ = sock_path.remove_file();

            let mut cmd = virtiofsd_command(binaries, share, &sock_path, &vfs_dir)?;
            debug!("virtiofsd: {:?}", cmd);
            let mut child = cmd.spawn().map_err(|e| {
                anyhow::anyhow!(messages::vm::virtiofsd_start_failed(&share.tag, e))
            })?;
            spawn_stderr_drain(&share.tag, &mut child);
            vm.virtiofsd_children.push((share.tag.clone(), child));
            wait_for_socket(&sock_path, &share.tag, &vm.virtiofsd_children)?;

            fs_args.push(format!(
                "tag={},socket={},num_queues=1,queue_size=1024",
                share.tag,
                sock_path.to_path()?.display()
            ));
        }

        let mut cmd = Command::new(binaries.cloud_hypervisor.to_path()?);
        cmd.arg("--kernel").arg(config.kernel.to_path()?);
        cmd.arg("--initramfs").arg(config.initramfs.to_path()?);
        cmd.arg("--cmdline").arg(&config.kernel_cmdline);
        let cpus_arg = if config.kvm {
            format!("boot={},nested=on", config.cpus)
        } else {
            format!("boot={}", config.cpus)
        };
        cmd.arg("--cpus").arg(cpus_arg);
        let ram_mib = config.memory_bytes / (1024 * 1024);
        cmd.arg("--memory")
            .arg(format!("size={ram_mib}M,shared=on"));
        // The guest reports its free pages, and cloud-hypervisor gives them
        // back to the host. The balloon does not inflate (size 0), so the
        // guest can always use all of its memory again.
        cmd.arg("--balloon").arg("size=0,free_page_reporting=on");
        cmd.arg("--console").arg("off");
        // The serial output goes to the standard output, and a thread copies
        // it to the log with a size limit.
        cmd.arg("--serial").arg("tty");
        cmd.arg("--vsock").arg(format!(
            "cid=3,socket={}",
            vm.vsock_socket_path.to_path()?.display()
        ));
        cmd.arg("--api-socket")
            .arg(runtime_dir.join("ch-api.sock")?.to_path()?);
        if !fs_args.is_empty() {
            cmd.arg("--fs");
            for fs in &fs_args {
                cmd.arg(fs);
            }
        }
        if let Some(cache_disk) = &config.cache_disk {
            cmd.arg("--disk").arg(format!(
                "path={},image_type=raw",
                cache_disk.to_path()?.display()
            ));
        }
        // cloud-hypervisor makes its Landlock rules from its configuration.
        if landlock_abi >= CH_LANDLOCK_MIN_ABI {
            cmd.arg("--landlock");
        }

        let serial_log = open_log(runtime_dir, "serial.log")?;
        let ch_log = open_log(runtime_dir, "cloud-hypervisor.log")?;
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        crate::util::process::tie_to_airlock(&mut cmd);

        debug!("cloud-hypervisor: {:?}", cmd);
        let mut ch_child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!(messages::vm::cloud_hypervisor_start_failed(e)))?;
        if let Some(out) = ch_child.stdout.take() {
            spawn_limited_log("serial", out, serial_log);
        }
        if let Some(err) = ch_child.stderr.take() {
            vm.ch_log_thread = spawn_limited_log("ch", err, ch_log);
        }
        vm.ch_child = Some(ch_child);
        Ok(vm)
    }

    /// Find a process of the VM that stopped.
    /// Returns:
    ///   The first process that stopped, or `None` if all run.
    fn stopped(&self) -> Option<VmProcess> {
        let exited = |c: &Child| crate::util::process::exited_unreaped(c.id());
        if self.ch_child.as_ref().is_some_and(exited) {
            return Some(VmProcess::Hypervisor);
        }
        exited_virtiofsd(&self.virtiofsd_children)
    }

    /// Make the error for a process of the VM that stopped without a
    /// request. For cloud-hypervisor, the error has the end of its log.
    /// Call it after [`Self::wait_ch_log`] or [`Self::wait_ch_log_async`],
    /// so that the log has all output. The guest can change the log, so the
    /// text has no control characters.
    fn stopped_error(&self, process: &VmProcess) -> String {
        match process {
            VmProcess::Hypervisor => {
                let tail = crate::util::strip_controls(log_tail(&self.ch_log).as_bytes());
                messages::vm::cloud_hypervisor_stopped(&self.ch_log, tail.trim_end())
            }
            VmProcess::Virtiofsd(tag) => messages::vm::virtiofsd_exited(tag),
        }
    }

    /// Check if the log thread of cloud-hypervisor wrote all output. The
    /// thread stops when cloud-hypervisor exits.
    fn ch_log_done(&self) -> bool {
        self.ch_log_thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    /// Wait up to 1 s until the log thread of cloud-hypervisor wrote all
    /// output. Blocks the thread: use it only out of async code.
    fn wait_ch_log(&self) {
        for _ in 0..LOG_WAIT_STEPS {
            if self.ch_log_done() {
                return;
            }
            std::thread::sleep(LOG_WAIT_STEP);
        }
    }

    /// Wait up to 1 s until the log thread of cloud-hypervisor wrote all
    /// output, without a block of the async runtime.
    async fn wait_ch_log_async(&self) {
        for _ in 0..LOG_WAIT_STEPS {
            if self.ch_log_done() {
                return;
            }
            tokio::time::sleep(LOG_WAIT_STEP).await;
        }
    }

    /// Open a vsock connection to the given guest port through the
    /// cloud-hypervisor vsock socket.
    /// Returns:
    ///   The connected socket in blocking mode. Error if the VM stopped, or
    ///   if the `CONNECT` handshake fails or times out. The caller tries
    ///   again on error.
    async fn vsock_connect(&self, port: u32) -> anyhow::Result<OwnedFd> {
        if let Some(process) = self.stopped() {
            self.wait_ch_log_async().await;
            return Err(VmStopped(self.stopped_error(&process)).into());
        }
        let mut stream = self
            .vsock_socket_path
            .connect_unix_async()
            .await
            .map_err(|e| {
                anyhow::anyhow!(messages::vm::vsock_socket_connect_failed(
                    &self.vsock_socket_path,
                    e
                ))
            })?;
        tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(&mut stream, port))
            .await
            .map_err(|_| anyhow::anyhow!(messages::vm::vsock_handshake_timeout(port)))??;
        // Blocking mode, like the other backends.
        let stream = stream.into_std()?;
        stream.set_nonblocking(false)?;
        Ok(OwnedFd::from(stream))
    }

    /// Kill cloud-hypervisor and all virtiofsd processes, and reap each one.
    /// A second call has nothing to stop.
    /// Returns:
    ///   `Ok` if all processes were reaped and none stopped with an error
    ///   before the request. Otherwise the first failure, after all
    ///   processes were handled.
    fn stop(&mut self) -> anyhow::Result<()> {
        let mut children: Vec<(VmProcess, Child)> = Vec::new();
        if let Some(child) = self.ch_child.take() {
            children.push((VmProcess::Hypervisor, child));
        }
        children.extend(
            self.virtiofsd_children
                .drain(..)
                .map(|(tag, c)| (VmProcess::Virtiofsd(tag), c)),
        );
        if children.is_empty() {
            return Ok(());
        }
        // A process that stopped with an error before the request failed.
        // Exit code 0 is a normal power off of the guest.
        let exited_before: Vec<bool> = children
            .iter()
            .map(|(_, c)| crate::util::process::exited_unreaped(c.id()))
            .collect();
        let mut result = Ok(());
        let mut crashed = None;
        for ((process, child), before) in children.into_iter().zip(exited_before) {
            match kill_and_reap(child, &process.to_string()) {
                Ok(status) if before && !status.success() => {
                    error!("{process} stopped without a request: {status}");
                    crashed.get_or_insert(process);
                }
                Ok(_) => {}
                Err(e) => {
                    if result.is_ok() {
                        result = Err(e);
                    }
                }
            }
        }
        cleanup_sockets(&self.runtime_dir);
        // All processes are reaped, so the log thread reads end of file.
        self.wait_ch_log();
        if let Some(process) = crashed
            && result.is_ok()
        {
            result = Err(anyhow::anyhow!(self.stopped_error(&process)));
        }
        result
    }
}

impl VmHandle for CloudHypervisorVm {
    fn vsock_connect(&self, port: u32) -> HandleFuture<'_, anyhow::Result<OwnedFd>> {
        Box::pin(CloudHypervisorVm::vsock_connect(self, port))
    }

    fn stop(&mut self) -> HandleFuture<'_, anyhow::Result<()>> {
        Box::pin(async move { CloudHypervisorVm::stop(self) })
    }
}

impl Drop for CloudHypervisorVm {
    fn drop(&mut self) {
        if let Err(e) = self.stop() {
            error!("{e:#}");
        }
    }
}

/// Make the virtiofsd command for `share`. If the kernel supports
/// Landlock, virtiofsd can reach only the share, its socket directory, its
/// own executable and the system libraries. When airlock runs as root,
/// virtiofsd can also read `/proc`.
/// Args:
///  - `binaries`: The executables of the backend
///  - `share`: The share to serve
///  - `sock_path`: The socket that virtiofsd makes
///  - `vfs_dir`: The directory of the socket
fn virtiofsd_command(
    binaries: &Binaries,
    share: &VmShare,
    sock_path: &VerifiedPath,
    vfs_dir: &VerifiedPath,
) -> anyhow::Result<Command> {
    let mut cmd = Command::new(binaries.virtiofsd.to_path()?);
    cmd.arg("--socket-path").arg(sock_path.to_path()?);
    cmd.arg("--shared-dir").arg(share.host_path.to_path()?);
    cmd.arg("--xattr");
    // The namespace sandbox of virtiofsd needs user namespaces, which many
    // hosts do not give to normal users. Landlock below is the limit.
    cmd.arg("--sandbox").arg("none");
    // File handles need the mount point of the share, which is out of the
    // Landlock limit. Only a process with `CAP_DAC_READ_SEARCH` can use
    // them at all.
    cmd.arg("--inode-file-handles=never");
    if share.read_only {
        cmd.arg("--readonly");
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    crate::util::process::tie_to_airlock(&mut cmd);

    if let Some(mut rules) = Ruleset::new()? {
        let access = if share.read_only {
            Access::Read
        } else {
            Access::ReadWrite
        };
        rules.allow(&share.host_path, access)?;
        // virtiofsd makes its socket and a lock file in the directory.
        rules.allow(vfs_dir, Access::ReadWrite)?;
        rules.allow(&binaries.virtiofsd, Access::Read)?;
        rules.allow_if_exists(VIRTIOFSD_SYSTEM_PATHS, Access::Read)?;
        // As root, virtiofsd reads the id maps of its own process to find
        // out if it must drop its supplemental groups. If the read fails,
        // the drop fails in a user namespace and virtiofsd stops. The rule
        // cannot name only the own process, so it is for all of `/proc`.
        // SAFETY: `geteuid` cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            rules.allow_if_exists(&["/proc"], Access::Read)?;
        }
        rules.apply_on_exec(&mut cmd);
    }
    Ok(cmd)
}

/// Do the vsock `CONNECT <port>` handshake of cloud-hypervisor on `stream`.
/// Returns:
///   Error if the guest refuses the connection or the answer is not
///   `OK <port>`.
async fn handshake(stream: &mut tokio::net::UnixStream, port: u32) -> anyhow::Result<()> {
    stream
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .await?;
    // Read one byte at a time. The guest can send data right after the
    // answer line, and a buffered read would take that data away.
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8];
        if stream.read(&mut byte).await? == 0 {
            // cloud-hypervisor closes the connection when no guest process
            // listens on the port.
            anyhow::bail!(messages::vm::vsock_handshake_failed(
                String::from_utf8_lossy(&line)
            ));
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > HANDSHAKE_MAX_LINE {
            anyhow::bail!(messages::vm::vsock_handshake_failed(
                String::from_utf8_lossy(&line)
            ));
        }
    }
    if !line.starts_with(b"OK ") {
        anyhow::bail!(messages::vm::vsock_handshake_failed(
            String::from_utf8_lossy(&line)
        ));
    }
    Ok(())
}

/// Send SIGKILL to `child` and wait for it. A kill error is only logged
/// (the process may have exited already).
/// Returns:
///   The exit status, or error if the wait fails, because then the exit
///   is not confirmed.
fn kill_and_reap(mut child: Child, name: &str) -> anyhow::Result<ExitStatus> {
    if let Err(e) = child.kill() {
        error!("{name} kill: {e}");
    }
    let status = child
        .wait()
        .map_err(|e| anyhow::anyhow!(messages::vm::child_wait_failed(name, e)))?;
    debug!("{name} exited: {status}");
    Ok(status)
}

/// Wait up to 5 s until the virtiofsd socket at `path` exists.
/// Args:
///  - `path`: The socket path
///  - `tag`: The share tag, for the errors
///  - `children`: The virtiofsd processes. The wait stops early when one
///    of them stops, with an error that names its share.
fn wait_for_socket(
    path: &VerifiedPath,
    tag: &str,
    children: &[(String, Child)],
) -> anyhow::Result<()> {
    for _ in 0..100 {
        if path.metadata().is_ok() {
            debug!("virtiofsd socket ready: {tag}");
            return Ok(());
        }
        if let Some(VmProcess::Virtiofsd(stopped)) = exited_virtiofsd(children) {
            anyhow::bail!(messages::vm::virtiofsd_stopped(stopped));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!(messages::vm::virtiofsd_socket_timeout(tag))
}

/// Find a virtiofsd process in `children` that stopped.
/// Returns:
///   The first virtiofsd that stopped, or `None` if all run.
fn exited_virtiofsd(children: &[(String, Child)]) -> Option<VmProcess> {
    children
        .iter()
        .find(|(_, c)| crate::util::process::exited_unreaped(c.id()))
        .map(|(tag, _)| VmProcess::Virtiofsd(tag.clone()))
}

/// Start a thread that reads the stderr of `child` and logs each line.
fn spawn_stderr_drain(tag: &str, child: &mut Child) {
    if let Some(stderr) = child.stderr.take() {
        let tag = tag.to_string();
        std::thread::Builder::new()
            .name(format!("vfs-{tag}"))
            .spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        // The default target, so that the debug log level
                        // shows the lines.
                        Ok(line) => debug!("virtiofsd [{tag}] {line}"),
                        Err(_) => break,
                    }
                }
            })
            .ok();
    }
}

/// Make a new, empty log file `name` in `dir`. An old file is removed.
fn open_log(dir: &VerifiedPath, name: &str) -> anyhow::Result<File> {
    let path = dir.join(name)?;
    remove_old(&path)?;
    Ok(path.create_new()?)
}

/// Start a thread that copies `from` to `to` up to `LOG_LIMIT` bytes. After
/// the limit, the thread reads and drops the rest, so the writer never
/// blocks.
/// Returns:
///   The thread handle, or `None` if the thread does not start.
fn spawn_limited_log(
    name: &str,
    mut from: impl Read + Send + 'static,
    mut to: File,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("{name}-log"))
        .spawn(move || {
            let copied = std::io::copy(&mut (&mut from).take(LOG_LIMIT), &mut to);
            if copied.is_ok_and(|n| n >= LOG_LIMIT) {
                let _ = writeln!(to, "\n{}", messages::vm::LOG_LIMIT_REACHED);
                let _ = std::io::copy(&mut from, &mut std::io::sink());
            }
        })
        .ok()
}

/// Read the last `LOG_TAIL_BYTES` of the log at `path`. The text starts at
/// a line start, if the tail has a line break.
/// Returns:
///   The text, or an empty text if the read fails.
fn log_tail(path: &VerifiedPath) -> String {
    use std::io::Seek;
    let read = || -> std::io::Result<String> {
        let mut file = path.open()?;
        let len = file.metadata()?.len();
        file.seek(std::io::SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)))?;
        let mut bytes = Vec::new();
        file.take(LOG_TAIL_BYTES).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if len > LOG_TAIL_BYTES
            && let Some((_, rest)) = text.split_once('\n')
        {
            return Ok(rest.to_string());
        }
        Ok(text)
    };
    read().unwrap_or_default()
}

/// Remove the cloud-hypervisor, vsock and virtiofsd sockets in the runtime
/// directory `dir`.
fn cleanup_sockets(dir: &VerifiedPath) {
    for name in ["vsock.sock", "ch-api.sock"] {
        if let Ok(path) = dir.join(name) {
            let _ = path.remove_file();
        }
    }
    // Remove the virtiofsd sockets. A `vfs` symlink is refused.
    let Ok(vfs_dir) = dir.join("vfs") else {
        return;
    };
    if let Ok(entries) = vfs_dir.read_dir() {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".sock")
                && let Ok(path) = vfs_dir.join(&name)
            {
                let _ = path.remove_file();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the start of VM helper processes and the vsock handshake.

    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    use std::time::Instant;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    use super::*;
    use crate::test_cfg::temp_dir;
    use crate::util::Roots;

    /// Find the `true` or `false` program of the host.
    fn program(name: &str) -> VerifiedPath {
        let path = [format!("/usr/bin/{name}"), format!("/bin/{name}")]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
            .unwrap();
        VerifiedPath::new_follow(path, None).unwrap()
    }

    /// Make a VM description with one share in `root`.
    fn vm_config(root: &std::path::Path) -> VmConfig {
        let roots = Roots::resolve(root, root, root).unwrap();
        let runtime = root.join("sandbox");
        let share = root.join("share");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::create_dir_all(&share).unwrap();
        VmConfig {
            cpus: 1,
            memory_bytes: 1 << 20,
            kernel: VerifiedPath::new("Image", &roots).unwrap(),
            initramfs: VerifiedPath::new("initramfs.gz", &roots).unwrap(),
            kernel_cmdline: String::new(),
            shares: vec![VmShare {
                tag: "project".to_string(),
                host_path: VerifiedPath::new(&share, &roots).unwrap(),
                read_only: false,
                immutable: false,
            }],
            cache_disk: None,
            runtime_dir: VerifiedPath::new(&runtime, &roots).unwrap(),
            asset_dir: VerifiedPath::new(root, &roots).unwrap(),
            kvm: false,
        }
    }

    /// Test that the VM start stops at once with a clear error when
    /// virtiofsd stops before its socket is ready. Without the check, the
    /// start waits for the full socket timeout.
    ///   1. Start the backend with a virtiofsd that exits at once
    ///   2. Check that the start fails fast with the virtiofsd error
    #[test]
    fn vm_start_with_virtiofsd_that_exits_fails_fast() {
        let tmp = temp_dir();
        let config = vm_config(tmp.path());
        let binaries = Binaries {
            cloud_hypervisor: program("true"),
            virtiofsd: program("false"),
        };

        let started = Instant::now();
        let err = CloudHypervisorVm::start(&config, &binaries)
            .err()
            .expect("start fails");

        assert_eq!(err.to_string(), messages::vm::virtiofsd_stopped("project"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// Test that the vsock handshake keeps the guest data that comes in the
    /// same packet as the answer. The guest sends the first RPC message at
    /// once after the accept.
    ///   1. Listen on the vsock socket like cloud-hypervisor
    ///   2. Answer the `CONNECT` with `OK` and guest data in one write
    ///   3. Connect and check that the socket gives the guest data
    #[tokio::test]
    async fn vsock_handshake_keeps_guest_data_after_answer() {
        let tmp = temp_dir();
        let config = vm_config(tmp.path());
        let vm = CloudHypervisorVm {
            ch_child: None,
            virtiofsd_children: Vec::new(),
            vsock_socket_path: config.runtime_dir.join("vsock.sock").unwrap(),
            runtime_dir: config.runtime_dir.clone(),
            ch_log: config.runtime_dir.join("cloud-hypervisor.log").unwrap(),
            ch_log_thread: None,
        };
        let listener = vm.vsock_socket_path.bind_unix_async().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = tokio::io::BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(line, "CONNECT 1024\n");
            stream.write_all(b"OK 1024\nGUEST DATA").await.unwrap();
        });

        let fd = vm.vsock_connect(1024).await.unwrap();
        server.await.unwrap();
        let mut data = String::new();
        std::fs::File::from(fd).read_to_string(&mut data).unwrap();

        assert_eq!(data, "GUEST DATA");
    }

    /// Test that a VM log stops at its size limit and that the copy still
    /// reads all output. The guest controls the serial output and must not
    /// fill the host disk. A writer that blocks would stop the VM.
    ///   1. Write more than the limit into the log pipe
    ///   2. Check that the write did not block and the log file has the
    ///      limit and the limit note
    #[test]
    fn log_output_above_limit_is_dropped() {
        let tmp = temp_dir();
        let config = vm_config(tmp.path());
        let file = open_log(&config.runtime_dir, "serial.log").unwrap();
        let (reader, mut writer) = std::io::pipe().unwrap();
        let thread = spawn_limited_log("test", reader, file).unwrap();

        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..LOG_LIMIT / 1024 / 1024 + 2 {
            writer.write_all(&chunk).unwrap();
        }
        drop(writer);
        thread.join().unwrap();

        let log = config.runtime_dir.join("serial.log").unwrap();
        let note = format!("\n{}\n", messages::vm::LOG_LIMIT_REACHED);
        assert_eq!(
            log.metadata().unwrap().size(),
            LOG_LIMIT + note.len() as u64
        );
    }

    /// Test that a process tied to airlock leads its own process group, so
    /// that a Ctrl+C of the terminal goes only to airlock.
    ///   1. Start a process tied to airlock
    ///   2. Read its process group and stop the process
    ///   3. Check that the group ID is the process ID and differs from the
    ///      group of the test
    #[test]
    fn process_tied_to_airlock_leads_its_own_process_group() {
        let mut cmd = Command::new("sleep");
        cmd.arg("10");
        crate::util::process::tie_to_airlock(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        let pgid = unsafe { libc::getpgid(pid) };
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(pgid, pid);
        assert_ne!(pgid, unsafe { libc::getpgrp() });
    }
}
