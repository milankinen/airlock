//! Cloud Hypervisor VM backend (Linux).
//!
//! Starts and stops the sandbox VM and its file sharing on Linux, and opens
//! connections from the host to the guest.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::OwnedFd;
use std::process::{Child, Command, Stdio};

use tracing::{debug, error};

use super::config::VmConfig;
use crate::messages;
use crate::util::{VerifiedPath, vfs};

/// Linux VM backend that uses cloud-hypervisor and virtiofsd.
pub struct CloudHypervisorBackend {
    ch_child: Option<Child>,
    virtiofsd_children: Vec<Child>,
    vsock_socket_path: VerifiedPath,
    runtime_dir: VerifiedPath,
}

impl CloudHypervisorBackend {
    /// Start one virtiofsd process for each VirtioFS share, then start
    /// cloud-hypervisor with vsock support.
    pub fn start(config: &VmConfig) -> anyhow::Result<Self> {
        let runtime_dir = &config.runtime_dir;
        let vfs_dir = runtime_dir.join("vfs")?;
        vfs::create_dir_all(&vfs_dir)?;
        let vsock_socket_path = runtime_dir.join("vsock.sock")?;

        cleanup_sockets(runtime_dir);

        let host_uid = unsafe { libc::getuid() };
        let host_gid = unsafe { libc::getgid() };

        // Start a virtiofsd process for each VirtioFS share.
        let mut virtiofsd_children = Vec::new();
        let mut fs_args: Vec<String> = Vec::new();

        for share in &config.shares {
            // Use only the last part of the tag (`files/rw` -> `rw`). Short
            // names keep the socket paths in the Unix socket limit. The last
            // parts of the tags are unique.
            let sock_name = share.tag.rsplit('/').next().unwrap_or(&share.tag);
            let sock_path = vfs_dir.join(format!("{sock_name}.sock"))?;
            let _ = vfs::remove_file(&sock_path);

            let mut cmd = Command::new(&config.virtiofsd);
            cmd.arg("--socket-path").arg(sock_path.to_path()?);
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
            let mut child = cmd.spawn().map_err(|e| {
                anyhow::anyhow!(messages::vm::virtiofsd_start_failed(&share.tag, e))
            })?;

            spawn_stderr_drain(&share.tag, &mut child);
            wait_for_socket(&sock_path, &share.tag)?;

            virtiofsd_children.push(child);

            fs_args.push(format!(
                "tag={},socket={},num_queues=1,queue_size=1024",
                share.tag,
                sock_path.to_path()?.display()
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
        let serial_log = runtime_dir.join("serial.log")?;
        // Cloud Hypervisor opens the file. Remove a symlink that the guest
        // put there, so that the open makes a new file.
        remove_old(&serial_log)?;
        cmd.arg("--console").arg("off");
        cmd.arg("--serial")
            .arg(format!("file={}", serial_log.to_path()?.display()));
        cmd.arg("--vsock").arg(format!(
            "cid=3,socket={}",
            vsock_socket_path.to_path()?.display()
        ));
        cmd.arg("--api-socket")
            .arg(runtime_dir.join("ch-api.sock")?.to_path()?);

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

        let ch_log = runtime_dir.join("cloud-hypervisor.log")?;
        remove_old(&ch_log)?;
        let log_file = vfs::create_new(&ch_log)?;
        let log_file_err = log_file.try_clone()?;

        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::from(log_file));
        cmd.stderr(Stdio::from(log_file_err));
        tie_to_airlock(&mut cmd);

        debug!("cloud-hypervisor: {:?}", cmd);

        let ch_child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!(messages::vm::cloud_hypervisor_start_failed(e)))?;

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
        let mut stream = vfs::connect_unix(&self.vsock_socket_path).map_err(|e| {
            anyhow::anyhow!(messages::vm::vsock_socket_connect_failed(
                &self.vsock_socket_path,
                e
            ))
        })?;

        // Cloud Hypervisor vsock needs a `CONNECT <port>` handshake.
        writeln!(stream, "CONNECT {port}")?;

        // Read the response line (`OK <port>`).
        let mut reader = BufReader::new(&stream);
        let mut response = String::new();
        reader.read_line(&mut response)?;
        if !response.starts_with("OK") {
            anyhow::bail!(messages::vm::vsock_handshake_failed(response));
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
        cleanup_sockets(&self.runtime_dir);
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
        .map_err(|e| anyhow::anyhow!(messages::vm::child_wait_failed(name, e)))?;
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

/// Remove the old entry at `path`, if any. Never follows a symlink.
/// A directory is removed too, so a planted directory cannot block a start.
fn remove_old(path: &VerifiedPath) -> std::io::Result<()> {
    let is_dir = vfs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    let result = if is_dir {
        vfs::remove_dir_all(path)
    } else {
        vfs::remove_file(path)
    };
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Wait up to 5 s until the virtiofsd socket at `path` exists.
fn wait_for_socket(path: &VerifiedPath, tag: &str) -> anyhow::Result<()> {
    for _ in 0..100 {
        if vfs::metadata(path).is_ok() {
            debug!("virtiofsd socket ready: {tag}");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!(messages::vm::virtiofsd_socket_timeout(tag))
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

/// Remove the cloud-hypervisor, vsock and virtiofsd sockets in the runtime
/// directory `dir`.
fn cleanup_sockets(dir: &VerifiedPath) {
    // `vsock.sock_<port>` is the socket of the supervisor port.
    let supervisor = format!("vsock.sock_{}", airlock_common::SUPERVISOR_PORT);
    for name in ["vsock.sock", "ch-api.sock", supervisor.as_str()] {
        if let Ok(path) = dir.join(name) {
            let _ = vfs::remove_file(&path);
        }
    }
    // Remove the virtiofsd sockets. A `vfs` symlink is refused.
    let Ok(vfs_dir) = dir.join("vfs") else {
        return;
    };
    if let Ok(entries) = vfs::read_dir(&vfs_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".sock")
                && let Ok(path) = vfs_dir.join(&name)
            {
                let _ = vfs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the start of VM helper processes.

    use super::*;
    use crate::test_cfg::temp_dir;
    use crate::util::Roots;

    /// Test that the VM start does not follow symlinks at the log files in
    /// the runtime dir. The guest can put them in a sandbox in the project
    /// to make the host write a host file.
    ///   1. Put symlinks to a host file at the serial and hypervisor logs
    ///   2. Start the backend with a hypervisor that exits at once
    ///   3. Check that the host file did not change and the logs are not
    ///      symlinks
    #[test]
    fn vm_start_does_not_follow_planted_log_links() {
        let tmp = temp_dir();
        let root = tmp.path();
        let victim = root.join("victim");
        std::fs::write(&victim, b"data").unwrap();
        let runtime = root.join("sandbox");
        std::fs::create_dir(&runtime).unwrap();
        for name in ["serial.log", "cloud-hypervisor.log"] {
            std::os::unix::fs::symlink(&victim, runtime.join(name)).unwrap();
        }
        let roots = Roots::resolve(root, root, root).unwrap();
        let config = VmConfig {
            cpus: 1,
            memory_bytes: 1 << 20,
            kernel: "Image".into(),
            initramfs: "initramfs.gz".into(),
            kernel_cmdline: String::new(),
            shares: Vec::new(),
            cache_disk: None,
            runtime_dir: VerifiedPath::new(&runtime, &roots).unwrap(),
            cloud_hypervisor: "true".into(),
            virtiofsd: "true".into(),
            kvm: false,
        };

        drop(CloudHypervisorBackend::start(&config).unwrap());
        assert_eq!(std::fs::read(&victim).unwrap(), b"data");
        for name in ["serial.log", "cloud-hypervisor.log"] {
            let md = std::fs::symlink_metadata(runtime.join(name));
            assert!(!md.is_ok_and(|m| m.file_type().is_symlink()), "{name}");
        }
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
