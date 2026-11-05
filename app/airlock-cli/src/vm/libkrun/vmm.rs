//! libkrun VM process.
//!
//! Reads the VM description from its input, loads the libkrun library,
//! builds the VM, locks itself in the Seatbelt sandbox and runs the VM. The
//! process stops when the guest stops, or when its input closes (airlock
//! stopped). It writes the libkrun log with the guest console, and its
//! errors, to its error output. The log has a size limit.

use std::ffi::{CStr, CString, c_char, c_void};
use std::io::{BufRead, Read, Write};
use std::os::unix::io::IntoRawFd;
use std::ptr;

use serde::{Deserialize, Serialize};

use super::seatbelt;

/// VM description that airlock gives to the VM process.
#[derive(Serialize, Deserialize)]
pub(super) struct VmmConfig {
    /// libkrun library.
    pub libkrun: String,
    /// Guest kernel (arm64 `Image`).
    pub kernel: String,
    /// Guest initramfs.
    pub initramfs: String,
    /// Kernel command line.
    pub cmdline: String,
    /// Number of virtual CPUs.
    pub cpus: u8,
    /// Guest memory size in MiB.
    pub ram_mib: u32,
    /// VirtioFS shares.
    pub shares: Vec<VmmShare>,
    /// Raw disk image for the cache volume.
    pub disk: Option<String>,
    /// Unix socket for each guest vsock port that the host connects to.
    pub vsock_ports: Vec<(u32, String)>,
    /// Enable nested virtualization.
    pub nested_virt: bool,
}

/// A host directory that the VM process shares with the guest.
#[derive(Serialize, Deserialize)]
pub(super) struct VmmShare {
    /// VirtioFS tag.
    pub tag: String,
    /// Shared directory on the host.
    pub path: String,
    /// If `true`, the guest cannot write to the share.
    pub read_only: bool,
    /// If `true`, the guest keeps file data in its cache across opens.
    pub immutable: bool,
}

/// libkrun object handle.
type Handle = *mut c_void;

/// Place where a libkrun call puts its error. Null after a successful call.
type ErrOut = *mut Handle;

/// libkrun string: UTF-8 bytes and their length (no null at the end).
#[repr(C)]
#[derive(Clone, Copy)]
struct KrunStr {
    data: *const c_char,
    len: usize,
}

impl KrunStr {
    /// Borrow `s`. `s` must live until the call ends.
    fn new(s: &str) -> Self {
        Self {
            data: s.as_ptr().cast(),
            len: s.len(),
        }
    }
}

/// Temporary handle of an object that the caller implements (`KrunVtableHandle`).
#[repr(C)]
struct VtableHandle {
    type_tag: u32,
    metadata: u32,
    vtable_ptr: *const c_void,
    user_data: *const c_void,
    vtable_size: u16,
}

/// Functions of a string writer (`KrunPushStrVtable`).
#[repr(C)]
struct PushStrVtable {
    drop: Option<unsafe extern "C" fn(*mut c_void)>,
    push: unsafe extern "C" fn(*mut c_void, KrunStr) -> bool,
}

/// Type tag of a string writer (`KRUN_PUSH_STR_TYPE_TAG`).
const PUSH_STR_TYPE_TAG: u32 = 16_777_228;
/// `KRUN_KERNEL_FORMAT_RAW`: uncompressed arm64 `Image`.
const KERNEL_FORMAT_RAW: u32 = 1;
/// `KRUN_DISK_FORMAT_RAW`.
const DISK_FORMAT_RAW: u32 = 0;
/// `KRUN_FREE_PAGE_REPORTING_MODE_REPLACE`: give the memory of free guest
/// pages back to macOS.
const FREE_PAGE_REPORTING_REPLACE: u32 = 1;
/// `KRUN_CACHE_POLICY_ALWAYS`: the guest keeps file data across opens.
const CACHE_POLICY_ALWAYS: u32 = 2;
/// `KRUN_LOG_LEVEL_WARN`.
const LOG_LEVEL_WARN: u32 = 2;
/// `KRUN_LOG_STYLE_NEVER`: no colors.
const LOG_STYLE_NEVER: u32 = 2;
/// `KRUN_LOG_OPTIONS_NO_ENV`: ignore `RUST_LOG`.
const LOG_OPTIONS_NO_ENV: u32 = 1;
/// Guest vsock CID.
const GUEST_CID: u64 = 3;
/// Value for "no file descriptor". The C API maps a negative fd to `None`
/// for its optional fd arguments (`Option<BorrowedFd>` in libkrun). The
/// libkrun examples give -1 to `krun_init_log` for the same reason.
const NO_FD: i32 = -1;
/// Maximum size of the log that the VM process writes. The log also has the
/// guest console, so the guest controls how much it gets.
const LOG_LIMIT: u64 = 16 * 1024 * 1024;

/// A loaded dynamic library. It stays loaded until the process ends.
struct Library(*mut c_void);

impl Library {
    /// Load the library at `path` and bind all its symbols now. Then no
    /// later call needs a file access, which the sandbox can deny.
    fn open(path: &str) -> Result<Self, String> {
        let c_path = CString::new(path).map_err(|e| format!("load {path}: {e}"))?;
        // SAFETY: `c_path` is null-terminated. The library is the libkrun
        // build that airlock contains. Its load runs no airlock code.
        let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!("load {path}: {}", dl_error()));
        }
        Ok(Self(handle))
    }

    /// Find the symbol `name`.
    fn symbol(&self, name: &CStr) -> Result<*mut c_void, String> {
        // SAFETY: the handle is open, and `name` is null-terminated.
        let sym = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        if sym.is_null() {
            return Err(format!("symbol {}: {}", name.to_string_lossy(), dl_error()));
        }
        Ok(sym)
    }
}

/// Get the last error text of the dynamic loader.
fn dl_error() -> String {
    // SAFETY: `dlerror` gives a null or a null-terminated text that stays
    // valid until the next loader call on this thread.
    unsafe {
        let err = libc::dlerror();
        if err.is_null() {
            "unknown error".to_string()
        } else {
            CStr::from_ptr(err).to_string_lossy().into_owned()
        }
    }
}

/// Declare the libkrun functions that the VM process uses, and a loader that
/// finds them in the library.
macro_rules! krun_api {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        /// libkrun functions.
        struct Api {
            $($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Api {
            /// Find all functions in `lib`.
            /// SAFETY: `lib` must be libkrun with the C API that this file
            /// declares.
            unsafe fn load(lib: &Library) -> Result<Self, String> {
                Ok(Self {
                    // SAFETY: the symbol is a function with this signature
                    // (see above).
                    $($name: unsafe {
                        std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(
                            lib.symbol(&CString::new(stringify!($name)).expect("no null in name"))?,
                        )
                    },)*
                })
            }
        }
    };
}

krun_api! {
    krun_init_log: fn(i32, u32, u32, u32, ErrOut) -> u64;
    krun_error_message: fn(Handle, *const VtableHandle);
    krun_error_destroy: fn(Handle);
    krun_mmio_device_manager_new: fn() -> Handle;
    krun_mmio_device_manager_add: fn(Handle, Handle);
    krun_fs_device_new: fn(KrunStr, KrunStr, ErrOut) -> Handle;
    krun_fs_device_new_read_only: fn(KrunStr, KrunStr, ErrOut) -> Handle;
    krun_fs_device_set_cache_policy: fn(Handle, u32, ErrOut) -> u64;
    krun_block_device_new: fn(KrunStr, KrunStr, u32, ErrOut) -> Handle;
    krun_vsock_device_new: fn(u64, u32, ErrOut) -> Handle;
    krun_vsock_device_add_unix_port: fn(Handle, u32, KrunStr, bool);
    krun_balloon_device_new: fn(ErrOut) -> Handle;
    krun_balloon_device_set_free_page_reporting_mode: fn(Handle, u32);
    krun_rng_device_new: fn(ErrOut) -> Handle;
    krun_console_device_builder: fn() -> Handle;
    krun_console_builder_add_default_console: fn(Handle, i32, i32, i32, ErrOut) -> u64;
    krun_console_builder_build: fn(Handle, ErrOut) -> Handle;
    krun_payload_load_external: fn(KrunStr, u32, KrunStr, KrunStr, ErrOut) -> Handle;
    krun_vmm_builder_new: fn() -> Handle;
    krun_vmm_builder_vcpus: fn(*mut Handle, u8, ErrOut) -> u64;
    krun_vmm_builder_ram_mib: fn(*mut Handle, u32, ErrOut) -> u64;
    krun_vmm_builder_payload: fn(*mut Handle, Handle);
    krun_vmm_builder_devices: fn(*mut Handle, Handle);
    krun_vmm_builder_nested_virt: fn(*mut Handle, bool);
    krun_vmm_builder_build: fn(*mut Handle, ErrOut) -> Handle;
    krun_vmm_run: fn(Handle);
}

impl Api {
    /// Run the libkrun call `f` with an error place.
    /// Returns:
    ///   The result of `f`, or the libkrun error text with `what` before it.
    fn call<T>(&self, what: &str, f: impl FnOnce(ErrOut) -> T) -> Result<T, String> {
        let mut err: Handle = ptr::null_mut();
        let out = f(&raw mut err);
        if err.is_null() {
            return Ok(out);
        }
        let mut text = String::new();
        let vtable = PushStrVtable {
            drop: None,
            push: push_to_string,
        };
        let writer = VtableHandle {
            type_tag: PUSH_STR_TYPE_TAG,
            metadata: 0,
            vtable_ptr: (&raw const vtable).cast(),
            user_data: (&raw mut text).cast_const().cast(),
            vtable_size: u16::try_from(size_of::<PushStrVtable>()).expect("small vtable"),
        };
        // SAFETY: `err` is an error from libkrun. The writer and its data
        // live until the call ends. The error is not used after the destroy.
        unsafe {
            (self.krun_error_message)(err, &raw const writer);
            (self.krun_error_destroy)(err);
        }
        Err(format!("{what}: {text}"))
    }

    /// Run the libkrun call `f` that makes an object.
    /// Returns:
    ///   The object handle, or error if libkrun gives an error or no object.
    fn object(&self, what: &str, f: impl FnOnce(ErrOut) -> Handle) -> Result<Handle, String> {
        not_null(what, self.call(what, f)?)
    }
}

/// Check that libkrun gave an object.
/// Returns:
///   `handle`, or error with `what` if it is null.
fn not_null(what: &str, handle: Handle) -> Result<Handle, String> {
    if handle.is_null() {
        Err(format!("{what}: libkrun gave no object"))
    } else {
        Ok(handle)
    }
}

/// Make a pipe for the libkrun log. A thread copies the log to the error
/// output (fd 2) until `LOG_LIMIT`, then drops the rest. libkrun never
/// waits on a full log.
/// Returns:
///   The write end of the pipe. It stays open until the process ends.
fn limited_log() -> Result<i32, String> {
    let (mut reader, writer) = std::io::pipe().map_err(|e| format!("log pipe: {e}"))?;
    std::thread::spawn(move || {
        let mut out = std::io::stderr();
        let _ = std::io::copy(&mut (&mut reader).take(LOG_LIMIT), &mut out);
        let _ = writeln!(
            out,
            "libkrun: log limit reached, more log lines are dropped"
        );
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    Ok(writer.into_raw_fd())
}

/// String writer for libkrun error texts. Adds `s` to the `String` at `data`.
unsafe extern "C" fn push_to_string(data: *mut c_void, s: KrunStr) -> bool {
    // SAFETY: `data` is the `String` of `Api::call`, and libkrun gives
    // `s.len` bytes at `s.data`.
    unsafe {
        let text = &mut *data.cast::<String>();
        let bytes = std::slice::from_raw_parts(s.data.cast::<u8>(), s.len);
        text.push_str(&String::from_utf8_lossy(bytes));
    }
    true
}

/// Entry point of the VM process. Never returns.
pub fn main() -> ! {
    let mut line = String::new();
    let config = std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| e.to_string())
        .and_then(|_| serde_json::from_str::<VmmConfig>(&line).map_err(|e| e.to_string()));
    let config = config.and_then(resolve_paths);
    let config = match config {
        Ok(config) => config,
        Err(e) => {
            eprintln!("libkrun: bad VM description: {e}");
            std::process::exit(1);
        }
    };

    // macOS has no parent death signal. airlock keeps the input open while
    // it runs, so an end of input means that airlock stopped.
    std::thread::spawn(|| {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        // SAFETY: stop at once. The VM threads of libkrun do not stop on a
        // normal exit.
        unsafe { libc::_exit(0) }
    });

    let Err(e) = run(&config);
    eprintln!("libkrun: {e}");
    std::process::exit(1);
}

/// Build the VM from `config`, apply the sandbox and run the VM.
/// Returns:
///   Only an error. On success, libkrun ends the process when the guest
///   stops.
fn run(config: &VmmConfig) -> Result<std::convert::Infallible, String> {
    let lib = Library::open(&config.libkrun)?;
    // SAFETY: the library is the libkrun build that airlock contains. It
    // stays loaded, because `Library` never closes it.
    let api = unsafe { Api::load(&lib) }?;

    // libkrun writes its log through a size limit to the error output.
    let log_fd = limited_log()?;
    // SAFETY (all calls below): the handles come from libkrun, and each
    // string lives in `config` or in a local until the call ends. Device and
    // payload handles move to libkrun on the add calls.
    api.call("init log", |e| unsafe {
        (api.krun_init_log)(
            log_fd,
            LOG_LEVEL_WARN,
            LOG_STYLE_NEVER,
            LOG_OPTIONS_NO_ENV,
            e,
        )
    })?;

    let devices = not_null("devices", unsafe { (api.krun_mmio_device_manager_new)() })?;
    let add = |device: Handle| unsafe { (api.krun_mmio_device_manager_add)(devices, device) };

    for share in &config.shares {
        let new = if share.read_only {
            api.krun_fs_device_new_read_only
        } else {
            api.krun_fs_device_new
        };
        let fs = api.object(&format!("share {}", share.tag), |e| unsafe {
            new(KrunStr::new(&share.tag), KrunStr::new(&share.path), e)
        })?;
        // The default cache policy drops the cached data of a file on each
        // open, because the host can change the file.
        if share.immutable {
            api.call(&format!("cache policy {}", share.tag), |e| unsafe {
                (api.krun_fs_device_set_cache_policy)(fs, CACHE_POLICY_ALWAYS, e)
            })?;
        }
        add(fs);
    }

    if let Some(disk) = &config.disk {
        let block = api.object("disk", |e| unsafe {
            (api.krun_block_device_new)(
                KrunStr::new("disk"),
                KrunStr::new(disk),
                DISK_FORMAT_RAW,
                e,
            )
        })?;
        add(block);
    }

    // No TSI: the guest has no network through vsock. libkrun listens on a
    // Unix socket for each port and connects the host to the guest port.
    let vsock = api.object("vsock", |e| unsafe {
        (api.krun_vsock_device_new)(GUEST_CID, 0, e)
    })?;
    for (port, path) in &config.vsock_ports {
        unsafe { (api.krun_vsock_device_add_unix_port)(vsock, *port, KrunStr::new(path), true) };
    }
    add(vsock);

    let balloon = api.object("balloon", |e| unsafe { (api.krun_balloon_device_new)(e) })?;
    unsafe {
        (api.krun_balloon_device_set_free_page_reporting_mode)(
            balloon,
            FREE_PAGE_REPORTING_REPLACE,
        );
    }
    add(balloon);

    let rng = api.object("rng", |e| unsafe { (api.krun_rng_device_new)(e) })?;
    add(rng);

    // Port 0 of the virtio console is `hvc0` in the guest. Without host
    // fds, libkrun writes its output to the log, one line for each guest
    // line. libkrun makes `hvc0` a console only for this port or for a
    // terminal. `NO_FD` for all streams adds no extra ports.
    let console = not_null("console", unsafe { (api.krun_console_device_builder)() })?;
    api.call("console port", |e| unsafe {
        (api.krun_console_builder_add_default_console)(console, NO_FD, NO_FD, NO_FD, e)
    })?;
    let console = api.object("console", |e| unsafe {
        (api.krun_console_builder_build)(console, e)
    })?;
    add(console);

    let payload = api.object("payload", |e| unsafe {
        (api.krun_payload_load_external)(
            KrunStr::new(&config.kernel),
            KERNEL_FORMAT_RAW,
            KrunStr::new(&config.initramfs),
            KrunStr::new(&config.cmdline),
            e,
        )
    })?;

    let mut builder = not_null("builder", unsafe { (api.krun_vmm_builder_new)() })?;
    api.call("vcpus", |e| unsafe {
        (api.krun_vmm_builder_vcpus)(&raw mut builder, config.cpus, e)
    })?;
    api.call("memory", |e| unsafe {
        (api.krun_vmm_builder_ram_mib)(&raw mut builder, config.ram_mib, e)
    })?;
    unsafe {
        (api.krun_vmm_builder_payload)(&raw mut builder, payload);
        (api.krun_vmm_builder_devices)(&raw mut builder, devices);
        if config.nested_virt {
            (api.krun_vmm_builder_nested_virt)(&raw mut builder, true);
        }
    }
    let vmm = api.object("build", |e| unsafe {
        (api.krun_vmm_builder_build)(&raw mut builder, e)
    })?;

    // Apply the sandbox as late as possible, but before the guest runs.
    // libkrun has loaded the library and opened the boot files and the disk
    // image, so the sandbox can deny all that the running VM does not need.
    // Never run the guest without it.
    seatbelt::apply(&policy(config))?;

    // libkrun ends the process when the guest stops.
    unsafe { (api.krun_vmm_run)(vmm) };
    Err("libkrun VM run returned".to_string())
}

/// Resolve the symlinks in the share and socket paths of `config`. libkrun
/// uses these paths after the sandbox applies, and the sandbox denies an
/// access through a symlink.
/// Returns:
///   The config with resolved paths, or error if a path does not resolve.
fn resolve_paths(mut config: VmmConfig) -> Result<VmmConfig, String> {
    for share in &mut config.shares {
        share.path = seatbelt::resolve(&share.path)?;
    }
    for (_, path) in &mut config.vsock_ports {
        *path = seatbelt::resolve(path)?;
    }
    Ok(config)
}

/// Make the sandbox policy for the VM of `config`: the shares and the
/// vsock sockets. The disk image, the kernel and the initramfs need no
/// rule, because libkrun opens them before the apply.
fn policy(config: &VmmConfig) -> seatbelt::Policy {
    let (ro, rw): (Vec<_>, Vec<_>) = config.shares.iter().partition(|s| s.read_only);
    let paths = |shares: Vec<&VmmShare>| shares.into_iter().map(|s| s.path.clone()).collect();
    seatbelt::Policy {
        read_only: paths(ro),
        read_write: paths(rw),
        unix_sockets: config.vsock_ports.iter().map(|(_, p)| p.clone()).collect(),
    }
}
