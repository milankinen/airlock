//! libkrun VM backend.
//!
//! Runs the sandbox VM with libkrun in a separate process of the airlock
//! program, because libkrun stops its process when the guest stops. Opens
//! connections from the host to the guest, and stops the VM. The VM process
//! also stops when airlock dies without a clean stop. The `airlock` binary
//! contains the libkrun library. The backend extracts it before the boot.
//!
//! When the VM process stops without a stop request, the errors give the
//! end of its log, which has the libkrun errors.

use std::io::{Read, Seek, Write};
use std::os::unix::io::{AsRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tracing::{debug, error, info};

use super::vmm::{VmmConfig, VmmShare};
use crate::messages;
use crate::settings::Vmm;
use crate::util::VerifiedPath;
use crate::vm::backend::{HandleFuture, VmHandle, VmStopped, VmmBackend};
use crate::vm::config::VmConfig;

/// VM backend that runs libkrun in a child process.
pub struct Libkrun;

impl VmmBackend for Libkrun {
    fn kind(&self) -> Vmm {
        Vmm::Libkrun
    }

    // The VM has no console device. The guest init opens its standard
    // files on `/dev/null` when the kernel has no console for it.
    fn kernel_console(&self) -> &'static str {
        ""
    }

    fn boot_vm<'a>(
        &'a self,
        config: &'a VmConfig,
    ) -> HandleFuture<'a, anyhow::Result<Box<dyn VmHandle>>> {
        Box::pin(async move {
            let library = extract_library(&config.asset_dir)?;
            let vm = LibkrunVm::start(config, &library)?;
            Ok(Box::new(vm) as Box<dyn VmHandle>)
        })
    }
}

/// Guest vsock ports that the host connects to.
const PORTS: [u32; 2] = [
    airlock_common::SUPERVISOR_PORT,
    airlock_common::NETWORK_PORT,
];

/// Time to wait for the guest to close a new vsock connection. libkrun
/// accepts the host connection before the guest answers. If no guest
/// process listens on the port, the guest refuses and libkrun closes the
/// host socket soon after.
///
/// The host connects only two times for each VM, both at boot. On the
/// network port, the guest sends first, so the wait ends at once. Only the
/// supervisor connect waits the full time, one time for each boot.
const REFUSE_WAIT: Duration = Duration::from_millis(200);

/// Maximum length of a Unix socket path on macOS, without the null at the
/// end (`sun_path` has 104 bytes).
const MAX_SOCKET_PATH: usize = 103;

/// Number of bytes from the end of the VM process log that go into the
/// error when the VM process stops without a request.
const LOG_TAIL_BYTES: u64 = 2048;

/// Extract the bundled libkrun library to the asset directory `dir` if its
/// checksum changed.
/// Returns:
///   The path of the library, or error if the extraction fails.
#[cfg(not(test))]
fn extract_library(dir: &VerifiedPath) -> anyhow::Result<VerifiedPath> {
    use crate::vm::assets::{AssetFile, extract_group};

    // A running VM process keeps its mapped copy of the old library, so
    // the rename in the extraction is safe.
    extract_group(
        dir,
        "libkrun",
        env!("AIRLOCK_LIBKRUN_CHECKSUM"),
        &[AssetFile {
            name: "libkrun.dylib",
            data: include_bytes!("../../../../../target/vm/libkrun.dylib"),
            mode: 0o644,
        }],
    )?;
    Ok(dir.join("libkrun.dylib")?)
}

/// Test stub. Tests do not extract the VM assets, so this always fails.
#[cfg(test)]
fn extract_library(_dir: &VerifiedPath) -> anyhow::Result<VerifiedPath> {
    anyhow::bail!("libkrun::extract_library not supported in tests")
}

/// A running libkrun VM process.
struct LibkrunVm {
    /// VM process. `None` after the stop.
    child: Option<Child>,
    /// Directory of the vsock sockets.
    runtime_dir: VerifiedPath,
    /// Log of the VM process.
    log: VerifiedPath,
}

impl LibkrunVm {
    /// Start the VM process. The guest boots in the background.
    /// Args:
    ///  - `config`: The VM to start. The vsock sockets and the log of the VM
    ///    process go to `config.runtime_dir`.
    ///  - `library`: The extracted libkrun library
    ///
    /// Returns:
    ///   The VM, or error if a path is not valid UTF-8, a socket path is
    ///   too long, or the VM process does not start.
    fn start(config: &VmConfig, library: &VerifiedPath) -> anyhow::Result<Self> {
        let runtime_dir = &config.runtime_dir;
        let mut vsock_ports = Vec::new();
        for port in PORTS {
            let path = socket_path(runtime_dir, port)?;
            // An old socket file from an earlier run blocks the bind. Remove
            // it first: the path check below refuses a symlink there.
            crate::vm::remove_old(&path)?;
            let path_str = utf8(&path)?;
            // libkrun only logs a bind error, and the connects then fail
            // without a clear reason. Stop here.
            if path_str.len() > MAX_SOCKET_PATH {
                anyhow::bail!(messages::libkrun::socket_path_too_long(
                    &path_str,
                    MAX_SOCKET_PATH
                ));
            }
            vsock_ports.push((port, path_str));
        }
        let vmm_config = VmmConfig {
            libkrun: utf8(library)?,
            kernel: utf8(&config.kernel)?,
            initramfs: utf8(&config.initramfs)?,
            cmdline: config.kernel_cmdline.clone(),
            cpus: u8::try_from(config.cpus)?,
            ram_mib: u32::try_from(config.memory_bytes / (1024 * 1024))?,
            shares: config
                .shares
                .iter()
                .map(|share| {
                    Ok(VmmShare {
                        tag: share.tag.clone(),
                        path: utf8(&share.host_path)?,
                        read_only: share.read_only,
                        immutable: share.immutable,
                    })
                })
                .collect::<anyhow::Result<_>>()?,
            disk: config.cache_disk.as_ref().map(utf8).transpose()?,
            vsock_ports,
            nested_virt: config.kvm,
        };
        let mut input = serde_json::to_vec(&vmm_config)?;
        input.push(b'\n');

        // The VM process writes the libkrun log to its error output. It limits the size. Remove an old entry first, so
        // that the create does not fail.
        let log = runtime_dir.join("libkrun.log")?;
        crate::vm::remove_old(&log)?;

        let exe = VerifiedPath::new_follow(std::env::current_exe()?, None)?;
        let mut cmd = Command::new(exe.to_path()?);
        cmd.args(crate::cli::cmd_internal::VMM_LIBKRUN_ARGS);
        // The VM process needs no environment. This keeps secrets and
        // `DYLD_*` variables out of it. libkrun ignores `RUST_LOG`.
        cmd.env_clear();
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::from(log.create_new()?));
        crate::util::process::tie_to_airlock(&mut cmd);
        debug!("libkrun: {cmd:?}");
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!(messages::libkrun::start_failed(e)))?;

        // Keep the input open after the write. macOS has no parent death
        // signal, so the VM process stops when its input closes.
        let written = child
            .stdin
            .as_mut()
            .expect("stdin is piped")
            .write_all(&input);
        let mut vm = Self {
            child: Some(child),
            runtime_dir: runtime_dir.clone(),
            log,
        };
        if let Err(e) = written {
            let _ = vm.kill();
            anyhow::bail!(messages::libkrun::start_failed(e));
        }
        Ok(vm)
    }

    /// Open a vsock connection to the given guest port through the Unix
    /// socket of the port.
    /// Args:
    ///  - `port`: Guest vsock port. Only the ports in `PORTS` have a socket.
    ///
    /// Returns:
    ///   The connected socket in blocking mode, like the other backends.
    ///   Error if the VM process stopped, the socket is not ready (boot), or
    ///   the guest refuses the connection (no listener yet). The caller
    ///   tries again on error.
    async fn vsock_connect(&self, port: u32) -> anyhow::Result<OwnedFd> {
        if let Some(child) = &self.child
            && crate::util::process::exited_unreaped(child.id())
        {
            return Err(VmStopped(self.stopped_error()).into());
        }
        let path = socket_path(&self.runtime_dir, port)?;
        // The VM process can create an entry at the socket path. The
        // connect must never follow a symlink there to an other socket of
        // the host, so connect only to a socket file.
        if !path
            .symlink_metadata()
            .is_ok_and(|m| std::os::unix::fs::FileTypeExt::is_socket(&m.file_type()))
        {
            anyhow::bail!(messages::libkrun::vsock_socket_missing(&path));
        }
        let stream = path.connect_unix_async().await.map_err(|e| {
            anyhow::anyhow!(messages::libkrun::vsock_socket_connect_failed(&path, e))
        })?;

        // A connection that the guest refuses gets an end of file. A
        // connection that the guest accepts gets data or nothing.
        if let Ok(ready) = tokio::time::timeout(REFUSE_WAIT, stream.readable()).await {
            ready?;
            let mut byte = 0u8;
            // SAFETY: the fd is open and the buffer has space for one byte.
            let n = unsafe {
                libc::recv(
                    stream.as_raw_fd(),
                    (&raw mut byte).cast(),
                    1,
                    libc::MSG_PEEK,
                )
            };
            if n == 0 {
                debug!("vsock port {port}: refused by guest");
                anyhow::bail!(messages::libkrun::vsock_refused(port));
            }
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(e.into());
                }
            }
        }
        // The tokio stream is non-blocking. Give the same blocking mode as
        // the other backends. The RPC transports set their own mode.
        let stream = stream.into_std()?;
        stream.set_nonblocking(false)?;
        Ok(OwnedFd::from(stream))
    }

    /// Stop the VM process and reap it. A second call has nothing to stop.
    /// Returns:
    ///   Error if the VM process stopped with an error before the stop
    ///   request (for example a guest crash or a sandbox deny), or if the
    ///   wait fails, because then the stop is not confirmed.
    fn stop(&mut self) -> anyhow::Result<()> {
        // libkrun ends the process with exit code 0 when the guest powers
        // off. Other exits before the request are failures.
        let stopped_before = self
            .child
            .as_ref()
            .is_some_and(|c| crate::util::process::exited_unreaped(c.id()));
        let Some(status) = self.kill()? else {
            return Ok(());
        };
        if stopped_before && !status.success() {
            error!("libkrun VM process stopped without a request: {status}");
            anyhow::bail!(self.stopped_error());
        }
        Ok(())
    }

    /// Kill the VM process group and reap the process.
    /// Returns:
    ///   The exit status, `None` if the process was already reaped, or
    ///   error if the wait fails.
    fn kill(&mut self) -> anyhow::Result<Option<std::process::ExitStatus>> {
        let Some(mut child) = self.child.take() else {
            return Ok(None);
        };
        // If the group kill fails, kill at least the VM process, so that
        // the wait cannot block.
        if !crate::util::process::kill_group(child.id())
            && let Err(e) = child.kill()
        {
            error!("libkrun kill: {e}");
        }
        let status = child
            .wait()
            .map_err(|e| anyhow::anyhow!(messages::libkrun::wait_failed(e)))?;
        info!("libkrun VM process exited: {status}");
        for port in PORTS {
            if let Ok(path) = socket_path(&self.runtime_dir, port) {
                let _ = path.remove_file();
            }
        }
        Ok(Some(status))
    }

    /// Make the error for a VM process that stopped without a request. It
    /// has the log path and the end of the log. The guest can put text in
    /// the log (for example a device name), so the text has no control
    /// characters.
    fn stopped_error(&self) -> String {
        let tail = crate::util::strip_controls(log_tail(&self.log).as_bytes());
        messages::libkrun::stopped(&self.log, tail.trim_end())
    }
}

impl VmHandle for LibkrunVm {
    fn vsock_connect(&self, port: u32) -> HandleFuture<'_, anyhow::Result<OwnedFd>> {
        Box::pin(Self::vsock_connect(self, port))
    }

    fn stop(&mut self) -> HandleFuture<'_, anyhow::Result<()>> {
        Box::pin(async move { Self::stop(self) })
    }
}

impl Drop for LibkrunVm {
    fn drop(&mut self) {
        if let Err(e) = self.kill() {
            error!("{e:#}");
        }
    }
}

/// Read the last `LOG_TAIL_BYTES` of the log at `path`. The text starts at
/// a line start, if the tail has a line break.
/// Returns:
///   The text, or an empty text if the read fails.
fn log_tail(path: &VerifiedPath) -> String {
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

/// Get the Unix socket path of a guest vsock port in `dir`. The name is
/// short, because macOS limits socket paths to 104 bytes.
fn socket_path(dir: &VerifiedPath, port: u32) -> std::io::Result<VerifiedPath> {
    dir.join(format!("k{port}.sock"))
}

/// Get `path` as a UTF-8 string. The libkrun API accepts only UTF-8 paths.
/// The path has no symlink in any part, so it is a real path, as the
/// sandbox of the VM process needs (it checks this again).
fn utf8(path: &VerifiedPath) -> anyhow::Result<String> {
    path.to_path()?
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!(messages::libkrun::path_not_utf8(path)))
}
