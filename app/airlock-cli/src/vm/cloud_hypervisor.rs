//! Cloud Hypervisor VM backend (Linux).
//!
//! Starts and stops the sandbox VM and its file sharing on Linux, and opens
//! connections from the host to the guest.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tracing::{debug, error};

use super::config::VmConfig;

/// Linux VM backend that uses cloud-hypervisor and virtiofsd.
pub struct CloudHypervisorBackend {
    ch_child: Option<Child>,
    virtiofsd_children: Vec<Child>,
    vsock_socket_path: PathBuf,
    runtime_dir: PathBuf,
}

impl CloudHypervisorBackend {
    /// Start one virtiofsd process for each VirtioFS share, then start
    /// cloud-hypervisor with vsock support.
    pub fn start(config: &VmConfig) -> anyhow::Result<Self> {
        let runtime_dir = &config.runtime_dir;
        let vfs_dir = runtime_dir.join("vfs");
        std::fs::create_dir_all(&vfs_dir)?;
        let vsock_socket_path = runtime_dir.join("vsock.sock");

        cleanup_sockets(runtime_dir, &vfs_dir);

        let host_uid = unsafe { libc::getuid() };
        let host_gid = unsafe { libc::getgid() };

        // Start a virtiofsd process for each VirtioFS share.
        let mut virtiofsd_children = Vec::new();
        let mut fs_args: Vec<String> = Vec::new();

        for share in &config.shares {
            let sock_name = share.tag.replace('/', "-");
            let sock_path = vfs_dir.join(format!("{sock_name}.sock"));
            let _ = std::fs::remove_file(&sock_path);

            let mut cmd = Command::new(&config.virtiofsd);
            cmd.arg("--socket-path").arg(&sock_path);
            cmd.arg("--shared-dir").arg(&share.host_path);
            cmd.arg("--xattr");
            cmd.arg("--sandbox").arg("none");
            cmd.arg("--translate-uid")
                .arg(format!("map:0:{host_uid}:1"));
            cmd.arg("--translate-gid")
                .arg(format!("map:0:{host_gid}:1"));
            if share.read_only {
                cmd.arg("--readonly");
            }
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::piped());
            tie_to_airlock(&mut cmd);

            debug!("virtiofsd: {:?}", cmd);
            let mut child = cmd
                .spawn()
                .map_err(|e| anyhow::anyhow!("failed to start virtiofsd for {}: {e}", share.tag))?;

            spawn_stderr_drain(&share.tag, &mut child);
            wait_for_socket(&sock_path, &share.tag)?;

            virtiofsd_children.push(child);

            fs_args.push(format!(
                "tag={},socket={},num_queues=1,queue_size=1024",
                share.tag,
                sock_path.display()
            ));
        }

        // Make the cloud-hypervisor command.
        let ram_mib = config.memory_bytes / (1024 * 1024);

        let mut cmd = Command::new(&config.cloud_hypervisor);
        cmd.arg("--kernel").arg(&config.kernel);
        cmd.arg("--initramfs").arg(&config.initramfs);
        cmd.arg("--cmdline").arg(&config.kernel_cmdline);
        let cpus_arg = if config.kvm {
            format!("boot={},nested=on", config.cpus)
        } else {
            format!("boot={}", config.cpus)
        };
        cmd.arg("--cpus").arg(cpus_arg);
        cmd.arg("--memory")
            .arg(format!("size={ram_mib}M,shared=on"));
        let serial_log = config.runtime_dir.join("serial.log");
        cmd.arg("--console").arg("off");
        cmd.arg("--serial")
            .arg(format!("file={}", serial_log.display()));
        cmd.arg("--vsock")
            .arg(format!("cid=3,socket={}", vsock_socket_path.display()));
        cmd.arg("--api-socket").arg(runtime_dir.join("ch-api.sock"));

        // VirtioFS shares (each uses a virtiofsd socket).
        if !fs_args.is_empty() {
            cmd.arg("--fs");
            for fs in &fs_args {
                cmd.arg(fs);
            }
        }

        // Block device for the cache disk.
        if let Some(cache_disk) = &config.cache_disk {
            cmd.arg("--disk")
                .arg(format!("path={},image_type=raw", cache_disk.display()));
        }

        let ch_log = config.runtime_dir.join("cloud-hypervisor.log");
        let log_file = std::fs::File::create(&ch_log)?;
        let log_file_err = log_file.try_clone()?;

        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::from(log_file));
        cmd.stderr(Stdio::from(log_file_err));
        tie_to_airlock(&mut cmd);

        debug!("cloud-hypervisor: {:?}", cmd);

        let ch_child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to start cloud-hypervisor: {e}"))?;

        Ok(Self {
            ch_child: Some(ch_child),
            virtiofsd_children,
            vsock_socket_path,
            runtime_dir: runtime_dir.clone(),
        })
    }

    /// Open a vsock connection to the given guest port through the
    /// cloud-hypervisor vsock socket.
    /// Returns:
    ///   The connected socket, or error if the `CONNECT` handshake fails.
    pub fn vsock_connect(&self, port: u32) -> anyhow::Result<OwnedFd> {
        let mut stream = UnixStream::connect(&self.vsock_socket_path).map_err(|e| {
            anyhow::anyhow!("vsock connect to {}: {e}", self.vsock_socket_path.display())
        })?;

        // Cloud Hypervisor vsock needs a `CONNECT <port>` handshake.
        writeln!(stream, "CONNECT {port}")?;

        // Read the response line (`OK <port>`).
        let mut reader = BufReader::new(&stream);
        let mut response = String::new();
        reader.read_line(&mut response)?;
        if !response.starts_with("OK") {
            anyhow::bail!("vsock CONNECT failed: {response}");
        }

        Ok(OwnedFd::from(stream))
    }
}

impl CloudHypervisorBackend {
    /// Kill cloud-hypervisor and all virtiofsd processes, and reap each one.
    /// A second call has nothing to stop.
    /// Returns:
    ///   `Ok` if all processes were reaped. Otherwise the first failure,
    ///   after all processes were handled.
    pub fn stop(&mut self) -> anyhow::Result<()> {
        let mut result = Ok(());
        if let Some(child) = self.ch_child.take() {
            result = kill_and_reap(child, "cloud-hypervisor");
        }
        for child in self.virtiofsd_children.drain(..) {
            let reaped = kill_and_reap(child, "virtiofsd");
            if result.is_ok() {
                result = reaped;
            }
        }
        cleanup_sockets(&self.runtime_dir, &self.runtime_dir.join("vfs"));
        result
    }
}

impl Drop for CloudHypervisorBackend {
    fn drop(&mut self) {
        if let Err(e) = self.stop() {
            error!("{e:#}");
        }
    }
}

/// Send SIGKILL to `child` and wait for it. A kill error is only logged
/// (the process may have exited already).
/// Returns:
///   Error if the wait fails, because then the exit is not confirmed.
fn kill_and_reap(mut child: Child, name: &str) -> anyhow::Result<()> {
    if let Err(e) = child.kill() {
        error!("{name} kill: {e}");
    }
    let status = child
        .wait()
        .map_err(|e| anyhow::anyhow!("{name} wait: {e}"))?;
    debug!("{name} exited: {status}");
    Ok(())
}

/// Start `cmd` in its own process group, and stop it when airlock stops.
///  * The SIGINT of the terminal (Ctrl+C in cooked mode, e.g. during the
///    boot) goes only to airlock, not to the process. Airlock then stops the
///    VM in the correct order (the guest sync first).
///  * `PR_SET_PDEATHSIG` kills the process if airlock dies without a stop
///    (SIGKILL, crash). The signal fires when the *thread* that started the
///    process exits. The VM starts on the main thread (the current-thread
///    runtime). The `getppid` check covers the case where airlock dies
///    before the `prctl`.
fn tie_to_airlock(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = libc::pid_t::try_from(std::process::id()).expect("pid fits pid_t");
    cmd.process_group(0);
    // Safety: only async-signal-safe calls (two syscalls) after fork.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

/// Path of the per-port vsock socket (`<base>_<port>`).
fn vsock_port_path(base: &Path, port: u32) -> PathBuf {
    let mut path = base.as_os_str().to_owned();
    path.push(format!("_{port}"));
    PathBuf::from(path)
}

/// Wait up to 5 s until the virtiofsd socket at `path` exists.
fn wait_for_socket(path: &Path, tag: &str) -> anyhow::Result<()> {
    for _ in 0..100 {
        if path.exists() {
            debug!("virtiofsd socket ready: {tag}");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!("virtiofsd socket not ready after 5s: {tag}")
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
                        Ok(line) => debug!(target: "virtiofsd", "[{tag}] {line}"),
                        Err(_) => break,
                    }
                }
            })
            .ok();
    }
}

/// Remove the cloud-hypervisor, vsock and virtiofsd sockets.
fn cleanup_sockets(dir: &Path, vfs_dir: &Path) {
    let patterns = ["vsock.sock", "ch-api.sock"];
    for pat in &patterns {
        let _ = std::fs::remove_file(dir.join(pat));
    }
    // Remove the `vsock.sock_<port>` file of the supervisor port.
    let _ = std::fs::remove_file(vsock_port_path(
        &dir.join("vsock.sock"),
        airlock_common::SUPERVISOR_PORT,
    ));
    // Remove the virtiofsd sockets.
    if let Ok(entries) = std::fs::read_dir(vfs_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".sock") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the start of VM helper processes.

    use super::*;

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
        tie_to_airlock(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        let pgid = unsafe { libc::getpgid(pid) };
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(pgid, pid);
        assert_ne!(pgid, unsafe { libc::getpgrp() });
    }
}
