//! Apple Virtualization.framework backend (macOS).
//!
//! Uses `VZVirtualMachine` on a serial dispatch queue. All VM method calls
//! are dispatched to this queue, with oneshot channels bridging back to the
//! async world.

use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::panic::{AssertUnwindSafe, UnwindSafe};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSError, NSFileHandle, NSString, NSURL};
use objc2_virtualization::*;
use tracing::{debug, error, info};

use super::config::VmConfig;

struct PipeEnds {
    read: OwnedFd,
    write: OwnedFd,
}

/// Run `f` inside `objc2::exception::catch`, converting any thrown
/// Obj-C exception into an error string.
///
/// Without this wrapper, an `NSException` thrown by the Virtualization
/// framework would unwind through a Rust frame and trigger
/// "Rust cannot catch foreign exceptions" → `abort()` → SIGABRT.
/// That's the suspected cause of the silent-exit bug; catching the
/// exception here turns it into a cleanly-reportable error instead.
fn catch_obj<R, F: FnOnce() -> R + UnwindSafe>(f: F) -> std::result::Result<R, String> {
    objc2::exception::catch(f).map_err(|exc| match exc {
        Some(e) => format!("ObjC exception: {e:?}"),
        None => "ObjC exception (nil)".to_string(),
    })
}

/// Report an Obj-C exception (or any other error string) back to an
/// awaiting oneshot. Used from the outer error-recovery path of every
/// dispatch-queue callback.
fn deliver_err<T>(
    tx: &Mutex<Option<tokio::sync::oneshot::Sender<std::result::Result<T, String>>>>,
    msg: String,
) {
    if let Some(sender) = tx.lock().unwrap().take() {
        let _ = sender.send(Err(msg));
    }
}

/// Create a Unix pipe and return both ends as owned file descriptors.
fn create_pipe() -> anyhow::Result<PipeEnds> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(anyhow::anyhow!(std::io::Error::last_os_error()));
    }
    Ok(PipeEnds {
        read: unsafe { OwnedFd::from_raw_fd(fds[0]) },
        write: unsafe { OwnedFd::from_raw_fd(fds[1]) },
    })
}

/// macOS VM backend using the Apple Virtualization.framework.
#[allow(dead_code, clippy::used_underscore_binding)]
pub struct AppleVmBackend {
    /// Live pointer to the VM object, nulled by `Drop` before `_vm`
    /// releases the underlying retain. Every dispatch-queue callback
    /// loads this atomically and bails on null — so a callback that
    /// was queued before Drop but runs after never dereferences
    /// freed memory. Using `AtomicPtr` (not `usize`) makes the
    /// happens-before relationship between Drop's `swap` and the
    /// callback's `load` explicit.
    vm: std::sync::Arc<AtomicPtr<VZVirtualMachine>>,
    /// Holds the +1 retain count of the VM object. `Option` so Drop
    /// can `take()` it after nulling `vm` and stopping the VM, so
    /// the object is freed strictly *after* any concurrent callback
    /// has observed the null.
    _vm: Option<Retained<VZVirtualMachine>>,
    vm_queue: dispatch2::DispatchRetained<DispatchQueue>,
    host_to_guest_write: OwnedFd,
    guest_to_host_read: OwnedFd,
}

// Safety: the struct is only moved between tokio tasks on the same
// thread (current_thread runtime). All VM method calls are dispatched
// to the serial VM queue, which synchronises access. The `AtomicPtr`
// check in every callback means a dispatched callback cannot observe
// a freed VM.
unsafe impl Send for AppleVmBackend {}

impl AppleVmBackend {
    /// Create a new VM (not yet started) with the given configuration.
    pub fn new(config: &VmConfig) -> anyhow::Result<Self> {
        let host_to_guest = create_pipe()?;
        let guest_to_host = create_pipe()?;

        // Wrap VZ framework calls in `catch_obj` so an NSException thrown
        // during config construction / validation / VM init becomes a
        // Rust error instead of aborting the process.
        let vm_config = catch_obj(AssertUnwindSafe(|| unsafe {
            Self::create_vm_config(config, &host_to_guest, &guest_to_host)
        }))
        .map_err(|e| anyhow::anyhow!(e))??;

        catch_obj(AssertUnwindSafe(|| unsafe {
            vm_config
                .validateWithError()
                .map_err(|e| anyhow::anyhow!(format!("VM configuration validation failed: {e}")))
        }))
        .map_err(|e| anyhow::anyhow!(e))??;
        debug!("VM configuration validated");

        let vm_queue = DispatchQueue::new("com.airlock.vm", dispatch2::DispatchQueueAttr::SERIAL);

        let vm = catch_obj(AssertUnwindSafe(|| unsafe {
            VZVirtualMachine::initWithConfiguration_queue(
                VZVirtualMachine::alloc(),
                &vm_config,
                &vm_queue,
            )
        }))
        .map_err(|e| anyhow::anyhow!(e))?;

        let vm_raw = (&raw const *vm).cast_mut();

        // Drop the guest-side pipe ends on the host. The VM has its own
        // dup'd copies via NSFileHandle. When the VM shuts down and closes
        // them, read() on guest_to_host_read will get EOF.
        drop(host_to_guest.read);
        drop(guest_to_host.write);

        Ok(Self {
            vm: std::sync::Arc::new(AtomicPtr::new(vm_raw)),
            _vm: Some(vm),
            vm_queue,
            host_to_guest_write: host_to_guest.write,
            guest_to_host_read: guest_to_host.read,
        })
    }

    unsafe fn create_vm_config(
        config: &VmConfig,
        host_to_guest: &PipeEnds,
        guest_to_host: &PipeEnds,
    ) -> anyhow::Result<Retained<VZVirtualMachineConfiguration>> {
        unsafe {
            // Boot loader
            let kernel_path = config.kernel.to_string_lossy();
            let kernel_url = NSURL::fileURLWithPath(&NSString::from_str(&kernel_path));
            let boot_loader =
                VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &kernel_url);

            boot_loader.setCommandLine(&NSString::from_str(&config.kernel_cmdline));

            let initramfs_path = config.initramfs.to_string_lossy();
            let initramfs_url = NSURL::fileURLWithPath(&NSString::from_str(&initramfs_path));
            boot_loader.setInitialRamdiskURL(Some(&initramfs_url));

            debug!(cmdline = %config.kernel_cmdline, "boot loader configured");

            // VM configuration
            let vm_config = VZVirtualMachineConfiguration::new();
            vm_config.setBootLoader(Some(&boot_loader.into_super()));
            vm_config.setCPUCount(config.cpus as usize);
            vm_config.setMemorySize(config.memory_bytes);

            // Platform
            let platform = VZGenericPlatformConfiguration::new();
            vm_config.setPlatform(&platform.into_super());

            // Serial port (console) via pipes.
            // Dup the fds for NSFileHandle with closeOnDealloc:true so the VM
            // owns its copies. When the VM shuts down, these close, giving the
            // host relay EOF.
            let guest_read_fd = libc::dup(host_to_guest.read.as_raw_fd());
            let guest_write_fd = libc::dup(guest_to_host.write.as_raw_fd());
            let read_handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                NSFileHandle::alloc(),
                guest_read_fd,
                true,
            );
            let write_handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                NSFileHandle::alloc(),
                guest_write_fd,
                true,
            );

            let attachment =
                VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                    VZFileHandleSerialPortAttachment::alloc(),
                    Some(&read_handle),
                    Some(&write_handle),
                );

            let serial_port = VZVirtioConsoleDeviceSerialPortConfiguration::new();
            serial_port.setAttachment(Some(&attachment.into_super()));

            let serial_port_config: Retained<VZSerialPortConfiguration> = serial_port.into_super();
            let serial_ports = NSArray::from_retained_slice(&[serial_port_config]);
            vm_config.setSerialPorts(&serial_ports);

            // Entropy device
            let entropy = VZVirtioEntropyDeviceConfiguration::new();
            let entropy_config: Retained<VZEntropyDeviceConfiguration> = entropy.into_super();
            let entropy_devices = NSArray::from_retained_slice(&[entropy_config]);
            vm_config.setEntropyDevices(&entropy_devices);

            // Memory balloon device
            let balloon = VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new();
            let balloon_config: Retained<VZMemoryBalloonDeviceConfiguration> = balloon.into_super();
            let balloons = NSArray::from_retained_slice(&[balloon_config]);
            vm_config.setMemoryBalloonDevices(&balloons);

            // Vsock device (for host↔guest communication)
            let vsock = VZVirtioSocketDeviceConfiguration::new();
            let vsock_config: Retained<VZSocketDeviceConfiguration> = vsock.into_super();
            let vsock_devices = NSArray::from_retained_slice(&[vsock_config]);
            vm_config.setSocketDevices(&vsock_devices);

            // VirtioFS shares (bundle + mounts)
            if !config.shares.is_empty() {
                let mut fs_devices_vec = Vec::new();
                for share in &config.shares {
                    let abs_path = std::fs::canonicalize(&share.host_path)
                        .unwrap_or_else(|_| share.host_path.clone());
                    let url =
                        NSURL::fileURLWithPath(&NSString::from_str(&abs_path.to_string_lossy()));
                    let shared_dir = VZSharedDirectory::initWithURL_readOnly(
                        VZSharedDirectory::alloc(),
                        &url,
                        share.read_only,
                    );
                    let dir_share = VZSingleDirectoryShare::initWithDirectory(
                        VZSingleDirectoryShare::alloc(),
                        &shared_dir,
                    );
                    let fs_config = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                        VZVirtioFileSystemDeviceConfiguration::alloc(),
                        &NSString::from_str(&share.tag),
                    );
                    fs_config.setShare(Some(&dir_share.into_super()));
                    let fs_device: Retained<VZDirectorySharingDeviceConfiguration> =
                        fs_config.into_super();
                    fs_devices_vec.push(fs_device);
                }
                let fs_devices = NSArray::from_retained_slice(&fs_devices_vec);
                vm_config.setDirectorySharingDevices(&fs_devices);
            }

            // VirtIO block device for cache volume
            if let Some(disk_path) = &config.cache_disk {
                let abs_path =
                    std::fs::canonicalize(disk_path).unwrap_or_else(|_| disk_path.clone());
                let url = NSURL::fileURLWithPath(&NSString::from_str(&abs_path.to_string_lossy()));
                let attachment = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                    VZDiskImageStorageDeviceAttachment::alloc(),
                    &url,
                    false,
                )
                .map_err(|e| anyhow::anyhow!("cache disk attachment: {e}"))?;
                let block_config = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                    VZVirtioBlockDeviceConfiguration::alloc(),
                    &attachment.into_super(),
                );
                let storage: Retained<VZStorageDeviceConfiguration> = block_config.into_super();
                let storage_devices = NSArray::from_retained_slice(&[storage]);
                vm_config.setStorageDevices(&storage_devices);
                debug!("cache block device attached: {}", abs_path.display());
            }

            Ok(vm_config)
        } // unsafe
    }
}

impl Drop for AppleVmBackend {
    fn drop(&mut self) {
        // Take the pointer out of the atomic *before* doing anything
        // else. Any callback queued earlier that hasn't run yet will
        // see null on its `load()` and bail. `_vm` still holds the
        // retain so the object is alive during this Drop body.
        //
        // The exec_sync closure is required to be `Send`; raw pointers
        // are `!Send` so we move the value in as a `usize` and cast
        // back inside the queue callback (same trick as start/stop).
        let vm_raw = self.vm.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !vm_raw.is_null() {
            let vm_addr = vm_raw as usize;
            self.vm_queue.exec_sync(move || {
                // Swallow any ObjC exception in Drop — we're tearing
                // down and there's nowhere to report it to.
                let res = catch_obj(AssertUnwindSafe(|| unsafe {
                    let vm = &*(vm_addr as *const VZVirtualMachine);
                    if vm.canStop() {
                        let _ = vm.requestStopWithError();
                    }
                }));
                if let Err(e) = res {
                    error!("VM stop exception: {e}");
                }
            });
        }
        // Drop the retain *after* exec_sync — which, because the queue
        // is serial, guarantees no other callback is running on it.
        let _ = self._vm.take();
    }
}

impl AppleVmBackend {
    /// Boot the VM asynchronously via the dispatch queue.
    pub async fn start(&mut self) -> anyhow::Result<()> {
        info!("starting VM...");

        let (tx, rx) = tokio::sync::oneshot::channel::<std::result::Result<(), String>>();
        let tx = Arc::new(Mutex::new(Some(tx)));
        let tx_outer = tx.clone();
        let vm_ptr = self.vm.clone();

        self.vm_queue.exec_async(move || {
            let tx_body = tx.clone();
            let caught = catch_obj(AssertUnwindSafe(move || {
                let vm_raw = vm_ptr.load(Ordering::Acquire);
                if vm_raw.is_null() {
                    deliver_err(&tx_body, "VM dropped before start".into());
                    return;
                }
                let vm = unsafe { &*vm_raw };
                let tx_handler = tx_body.clone();
                let handler = RcBlock::new(move |err_ptr: *mut NSError| {
                    let caught = catch_obj(AssertUnwindSafe(|| {
                        if err_ptr.is_null() {
                            Ok(())
                        } else {
                            let err = unsafe { &*err_ptr };
                            Err(format!("{}", err.localizedDescription()))
                        }
                    }));
                    let result = caught.unwrap_or_else(Err);
                    if let Some(s) = tx_handler.lock().unwrap().take() {
                        let _ = s.send(result);
                    }
                });
                unsafe {
                    vm.startWithCompletionHandler(&handler);
                }
            }));
            if let Err(msg) = caught {
                deliver_err(&tx_outer, msg);
            }
        });

        rx.await
            .map_err(|_| anyhow::anyhow!("VM start channel closed"))?
            .map_err(|e| anyhow::anyhow!(format!("VM start failed: {e}")))?;

        info!("VM started");
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn stop(&mut self) -> anyhow::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel::<std::result::Result<(), String>>();
        let tx = Arc::new(Mutex::new(Some(tx)));
        let tx_outer = tx.clone();
        let vm_ptr = self.vm.clone();

        self.vm_queue.exec_async(move || {
            let tx_body = tx.clone();
            let caught = catch_obj(AssertUnwindSafe(move || {
                let vm_raw = vm_ptr.load(Ordering::Acquire);
                if vm_raw.is_null() {
                    deliver_err(&tx_body, "VM dropped before stop".into());
                    return;
                }
                let tx_handler = tx_body.clone();
                let handler = RcBlock::new(move |err_ptr: *mut NSError| {
                    let caught = catch_obj(AssertUnwindSafe(|| {
                        if err_ptr.is_null() {
                            Ok(())
                        } else {
                            let err = unsafe { &*err_ptr };
                            Err(format!("{err}"))
                        }
                    }));
                    let result = caught.unwrap_or_else(Err);
                    if let Some(s) = tx_handler.lock().unwrap().take() {
                        let _ = s.send(result);
                    }
                });
                unsafe {
                    let vm = &*vm_raw;
                    vm.stopWithCompletionHandler(&handler);
                }
            }));
            if let Err(msg) = caught {
                deliver_err(&tx_outer, msg);
            }
        });

        rx.await
            .map_err(|_| anyhow::anyhow!("VM stop channel closed"))?
            .map_err(|e| anyhow::anyhow!(format!("VM stop failed: {e}")))?;

        info!("VM stopped");
        Ok(())
    }

    /// Connect to a vsock port inside the VM, returning an owned fd.
    pub async fn vsock_connect(&self, port: u32) -> anyhow::Result<OwnedFd> {
        let (tx, rx) = tokio::sync::oneshot::channel::<std::result::Result<i32, String>>();
        let tx = Arc::new(Mutex::new(Some(tx)));
        let tx_outer = tx.clone();
        let vm_ptr = self.vm.clone();

        self.vm_queue.exec_async(move || {
            let tx_body = tx.clone();
            let caught = catch_obj(AssertUnwindSafe(move || {
                let vm_raw = vm_ptr.load(Ordering::Acquire);
                if vm_raw.is_null() {
                    deliver_err(&tx_body, "VM dropped".into());
                    return;
                }
                unsafe {
                    let vm = &*vm_raw;
                    let devices = vm.socketDevices();
                    let Some(device) = devices.firstObject_unchecked() else {
                        deliver_err(&tx_body, "no vsock device".into());
                        return;
                    };
                    // Downcast VZSocketDevice → VZVirtioSocketDevice
                    // Safety: we configured exactly one VZVirtioSocketDeviceConfiguration
                    let device_ptr =
                        std::ptr::from_ref::<VZSocketDevice>(device).cast::<VZVirtioSocketDevice>();
                    let device = &*device_ptr;

                    let tx_handler = tx_body.clone();
                    let handler = RcBlock::new(
                        move |conn_ptr: *mut VZVirtioSocketConnection, err_ptr: *mut NSError| {
                            let caught = catch_obj(AssertUnwindSafe(|| {
                                if err_ptr.is_null() && !conn_ptr.is_null() {
                                    let conn = &*conn_ptr;
                                    // Dup the fd so it outlives the connection.
                                    let fd = libc::dup(conn.fileDescriptor());
                                    if fd < 0 {
                                        Err("failed to dup vsock fd".into())
                                    } else {
                                        Ok(fd)
                                    }
                                } else if !err_ptr.is_null() {
                                    let err = &*err_ptr;
                                    Err(format!("{}", err.localizedDescription()))
                                } else {
                                    Err("vsock connect returned null".into())
                                }
                            }));
                            let result = caught.unwrap_or_else(Err);
                            if let Some(s) = tx_handler.lock().unwrap().take() {
                                let _ = s.send(result);
                            }
                        },
                    );
                    device.connectToPort_completionHandler(port, &handler);
                }
            }));
            if let Err(msg) = caught {
                deliver_err(&tx_outer, msg);
            }
        });

        let fd = rx
            .await
            .map_err(|_| anyhow::anyhow!("vsock connect channel closed"))?
            .map_err(|e| anyhow::anyhow!(format!("vsock connect failed: {e}")))?;

        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}
