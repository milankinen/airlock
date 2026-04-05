use std::ffi::CString;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use tracing::debug;

use super::config::VmConfig;

pub fn check_kvm_access() {
    let path = Path::new("/dev/kvm");
    if !path.exists() {
        crate::cli::error!("KVM not available (/dev/kvm not found)");
        crate::cli::error!("ensure KVM is enabled in your kernel/BIOS");
        std::process::exit(1);
    }
    if let Ok(metadata) = path.metadata() {
        let mode = metadata.mode();
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let dev_uid = metadata.uid();
        let dev_gid = metadata.gid();

        let owner_ok = uid == dev_uid && (mode & 0o600 == 0o600);
        let group_ok = gid == dev_gid && (mode & 0o060 == 0o060);
        let other_ok = mode & 0o006 == 0o006;

        // Also check supplementary groups
        let supp_ok = if group_ok {
            false
        } else {
            let mut groups = vec![0u32; 64];
            let n = unsafe { libc::getgroups(groups.len() as i32, groups.as_mut_ptr()) };
            n > 0 && groups[..n as usize].contains(&dev_gid)
        };

        if !owner_ok && !group_ok && !supp_ok && !other_ok {
            crate::cli::error!("no permission to access /dev/kvm");
            crate::cli::error!("run: sudo usermod -aG kvm $USER  (then re-login)");
            std::process::exit(1);
        }
    }
}

const KRUN_LOG_ERROR: u32 = 1;
const KRUN_LOG_DEBUG: u32 = 4;

fn check_krun(ret: i32, op: &str) -> anyhow::Result<()> {
    if ret < 0 {
        anyhow::bail!("krun {op} failed (error code: {ret})");
    }
    Ok(())
}

fn to_cstr(s: &str) -> anyhow::Result<CString> {
    CString::new(s).map_err(|e| anyhow::anyhow!("invalid C string: {e}"))
}

/// Function pointers loaded from libkrun.so via dlopen.
struct KrunFns {
    set_log_level: unsafe extern "C" fn(level: u32) -> i32,
    create_ctx: unsafe extern "C" fn() -> i32,
    set_vm_config: unsafe extern "C" fn(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> i32,
    set_root: unsafe extern "C" fn(ctx_id: u32, root_path: *const libc::c_char) -> i32,
    set_exec: unsafe extern "C" fn(
        ctx_id: u32,
        exec_path: *const libc::c_char,
        argv: *const *const libc::c_char,
        envp: *const *const libc::c_char,
    ) -> i32,
    add_virtiofs: unsafe extern "C" fn(
        ctx_id: u32,
        tag: *const libc::c_char,
        path: *const libc::c_char,
    ) -> i32,
    add_vsock_port2: unsafe extern "C" fn(
        ctx_id: u32,
        port: u32,
        filepath: *const libc::c_char,
        listen: bool,
    ) -> i32,
    start_enter: unsafe extern "C" fn(ctx_id: u32) -> i32,
}

impl KrunFns {
    fn load(libkrun_path: &Path, libkrunfw_path: &Path) -> anyhow::Result<Self> {
        // Load libkrunfw first — libkrun dlopen's it by soname, so we preload it
        let fw_path = to_cstr(&libkrunfw_path.to_string_lossy())?;
        let fw_handle =
            unsafe { libc::dlopen(fw_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if fw_handle.is_null() {
            let err = unsafe { std::ffi::CStr::from_ptr(libc::dlerror()) };
            anyhow::bail!("dlopen libkrunfw failed: {}", err.to_string_lossy());
        }
        debug!("loaded libkrunfw from {}", libkrunfw_path.display());

        let path = to_cstr(&libkrun_path.to_string_lossy())?;
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            let err = unsafe { std::ffi::CStr::from_ptr(libc::dlerror()) };
            anyhow::bail!("dlopen libkrun failed: {}", err.to_string_lossy());
        }
        debug!("loaded libkrun from {}", libkrun_path.display());

        unsafe {
            Ok(Self {
                set_log_level: load_sym(handle, "krun_set_log_level")?,
                create_ctx: load_sym(handle, "krun_create_ctx")?,
                set_vm_config: load_sym(handle, "krun_set_vm_config")?,
                set_root: load_sym(handle, "krun_set_root")?,
                set_exec: load_sym(handle, "krun_set_exec")?,
                add_virtiofs: load_sym(handle, "krun_add_virtiofs")?,
                add_vsock_port2: load_sym(handle, "krun_add_vsock_port2")?,
                start_enter: load_sym(handle, "krun_start_enter")?,
            })
        }
        // Intentionally never dlclose — keep libraries loaded for process lifetime
    }
}

unsafe fn load_sym<T>(handle: *mut libc::c_void, name: &str) -> anyhow::Result<T> {
    let cname = to_cstr(name)?;
    let ptr = unsafe { libc::dlsym(handle, cname.as_ptr()) };
    if ptr.is_null() {
        let err = unsafe { std::ffi::CStr::from_ptr(libc::dlerror()) };
        anyhow::bail!("dlsym {name} failed: {}", err.to_string_lossy());
    }
    Ok(unsafe { std::mem::transmute_copy(&ptr) })
}

pub struct KrunVmBackend {
    #[allow(dead_code)]
    vm_thread: Option<JoinHandle<()>>,
    stop_rx: Option<tokio::sync::watch::Receiver<bool>>,
    vsock_socket_path: PathBuf,
}

impl KrunVmBackend {
    pub fn start(
        config: &VmConfig,
        libkrun_path: &Path,
        libkrunfw_path: &Path,
    ) -> anyhow::Result<Self> {
        let fns = KrunFns::load(libkrun_path, libkrunfw_path)?;

        let log_level = if tracing::enabled!(tracing::Level::DEBUG) {
            KRUN_LOG_DEBUG
        } else {
            KRUN_LOG_ERROR
        };

        let vsock_socket_path = config.runtime_dir.join("vsock.sock");
        let _ = std::fs::remove_file(&vsock_socket_path);

        check_krun(unsafe { (fns.set_log_level)(log_level) }, "set_log_level")?;

        let ctx = unsafe { (fns.create_ctx)() };
        if ctx < 0 {
            anyhow::bail!("krun create_ctx failed (error code: {ctx})");
        }
        let ctx = ctx as u32;

        let ram_mib = (config.memory_bytes / (1024 * 1024)) as u32;
        check_krun(
            unsafe { (fns.set_vm_config)(ctx, config.cpus as u8, ram_mib) },
            "set_vm_config",
        )?;

        // Set root to our initramfs rootfs (extracted during rootfs build)
        let root_path = to_cstr(&config.initramfs_root.to_string_lossy())?;
        check_krun(
            unsafe { (fns.set_root)(ctx, root_path.as_ptr()) },
            "set_root",
        )?;

        let exec_path = to_cstr("/init")?;
        let env_strings = build_env(config);
        let env_cstrs: Vec<CString> = env_strings
            .iter()
            .map(|s| to_cstr(s))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut env_ptrs: Vec<*const libc::c_char> = env_cstrs.iter().map(|c| c.as_ptr()).collect();
        env_ptrs.push(std::ptr::null());

        check_krun(
            unsafe { (fns.set_exec)(ctx, exec_path.as_ptr(), std::ptr::null(), env_ptrs.as_ptr()) },
            "set_exec",
        )?;

        for share in &config.shares {
            let tag = to_cstr(&share.tag)?;
            let path = to_cstr(&share.host_path.to_string_lossy())?;
            check_krun(
                unsafe { (fns.add_virtiofs)(ctx, tag.as_ptr(), path.as_ptr()) },
                &format!("add_virtiofs({})", share.tag),
            )?;
            debug!(
                "virtiofs: tag={} path={}",
                share.tag,
                share.host_path.display()
            );
        }

        if config.cache_disk.is_some() {
            tracing::warn!("cache disk not supported on Linux yet (libkrun blk feature pending)");
        }

        let sock_path = to_cstr(&vsock_socket_path.to_string_lossy())?;
        check_krun(
            unsafe {
                (fns.add_vsock_port2)(
                    ctx,
                    ezpez_protocol::SUPERVISOR_PORT,
                    sock_path.as_ptr(),
                    true,
                )
            },
            "add_vsock_port2",
        )?;
        debug!(
            "vsock: port={} socket={}",
            ezpez_protocol::SUPERVISOR_PORT,
            vsock_socket_path.display()
        );

        // Start VM in a dedicated thread (krun_start_enter blocks)
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let start_enter = fns.start_enter;

        let handle = std::thread::Builder::new()
            .name("krun-vm".into())
            .spawn(move || {
                debug!("krun_start_enter (ctx={})", ctx);
                let ret = unsafe { start_enter(ctx) };
                if ret < 0 {
                    tracing::error!("krun_start_enter failed (error code: {})", ret);
                }
                let _ = stop_tx.send(true);
            })?;

        Ok(Self {
            vm_thread: Some(handle),
            stop_rx: Some(stop_rx),
            vsock_socket_path,
        })
    }

    pub fn vsock_connect(&self) -> anyhow::Result<OwnedFd> {
        let stream = UnixStream::connect(&self.vsock_socket_path)?;
        Ok(OwnedFd::from(stream))
    }

    pub async fn wait_for_stop_impl(&self) {
        if let Some(rx) = &self.stop_rx {
            let mut rx = rx.clone();
            let _ = rx.changed().await;
        }
    }
}

impl Drop for KrunVmBackend {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.vsock_socket_path);
    }
}

fn build_env(config: &VmConfig) -> Vec<String> {
    let tags: Vec<&str> = config.shares.iter().map(|s| s.tag.as_str()).collect();
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut env = vec![
        format!("EZPEZ_SHARES={}", tags.join(",")),
        format!("EZPEZ_EPOCH={epoch}"),
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
    ];
    if !config.host_ports.is_empty() {
        let ports: Vec<String> = config.host_ports.iter().map(ToString::to_string).collect();
        env.push(format!("EZPEZ_HOST_PORTS={}", ports.join(",")));
    }
    env
}
