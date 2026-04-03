use super::config::VmConfig;
use crate::error::CliError;

type Result<T> = std::result::Result<T, CliError>;

use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Mutex;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSError, NSFileHandle, NSString, NSURL};
use objc2_virtualization::*;
use tracing::{debug, info};

struct PipeEnds {
    read: OwnedFd,
    write: OwnedFd,
}

fn create_pipe() -> Result<PipeEnds> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(CliError::Unexpected(std::io::Error::last_os_error().into()));
    }
    Ok(PipeEnds {
        read: unsafe { OwnedFd::from_raw_fd(fds[0]) },
        write: unsafe { OwnedFd::from_raw_fd(fds[1]) },
    })
}

#[allow(dead_code)]
pub struct AppleVmBackend {
    /// Raw pointer to the VM object. Only accessed on the dispatch queue.
    /// Stored as usize to avoid Send issues with raw pointers.
    vm_ptr: usize,
    /// Prevent the Retained from being dropped while the VM is alive.
    _vm: Retained<VZVirtualMachine>,
    vm_queue: dispatch2::DispatchRetained<DispatchQueue>,
    host_to_guest_write: OwnedFd,
    guest_to_host_read: OwnedFd,
}

// Safety: all VM operations are dispatched to the serial VM queue.
// The struct itself is only moved between tokio tasks on the same thread
// (current_thread runtime). The raw pointer is only dereferenced on the queue.
unsafe impl Send for AppleVmBackend {}

impl AppleVmBackend {
    pub fn new(config: &VmConfig) -> Result<Self> {
        let host_to_guest = create_pipe()?;
        let guest_to_host = create_pipe()?;

        let vm_config = unsafe { Self::create_vm_config(config, &host_to_guest, &guest_to_host) };

        unsafe {
            vm_config.validateWithError().map_err(|e| {
                CliError::expected(format!("VM configuration validation failed: {e}"))
            })?;
        }
        debug!("VM configuration validated");

        let vm_queue = DispatchQueue::new("com.ezpez.vm", dispatch2::DispatchQueueAttr::SERIAL);

        let vm = unsafe {
            VZVirtualMachine::initWithConfiguration_queue(
                VZVirtualMachine::alloc(),
                &vm_config,
                &vm_queue,
            )
        };

        let vm_ptr = (&raw const *vm) as usize;

        // Drop the guest-side pipe ends on the host. The VM has its own
        // dup'd copies via NSFileHandle. When the VM shuts down and closes
        // them, read() on guest_to_host_read will get EOF.
        drop(host_to_guest.read);
        drop(guest_to_host.write);

        Ok(Self {
            vm_ptr,
            _vm: vm,
            vm_queue,
            host_to_guest_write: host_to_guest.write,
            guest_to_host_read: guest_to_host.read,
        })
    }

    unsafe fn create_vm_config(
        config: &VmConfig,
        host_to_guest: &PipeEnds,
        guest_to_host: &PipeEnds,
    ) -> Retained<VZVirtualMachineConfiguration> {
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

            vm_config
        } // unsafe
    }
}

impl Drop for AppleVmBackend {
    fn drop(&mut self) {
        // Best-effort synchronous stop via requestStop
        let vm_addr = self.vm_ptr;
        self.vm_queue.exec_sync(move || unsafe {
            let vm = &*(vm_addr as *const VZVirtualMachine);
            if vm.canStop() {
                let _ = vm.requestStopWithError();
            }
        });
    }
}

impl AppleVmBackend {
    pub async fn start(&mut self) -> Result<()> {
        info!("starting VM...");

        let (tx, rx) = tokio::sync::oneshot::channel::<std::result::Result<(), String>>();
        let tx = Mutex::new(Some(tx));
        let vm_addr = self.vm_ptr;

        self.vm_queue.exec_async(move || unsafe {
            let vm = &*(vm_addr as *const VZVirtualMachine);

            let handler = RcBlock::new(move |err_ptr: *mut NSError| {
                let result = if err_ptr.is_null() {
                    Ok(())
                } else {
                    let err = &*err_ptr;
                    Err(format!("{}", err.localizedDescription()))
                };
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(result);
                }
            });
            vm.startWithCompletionHandler(&handler);
        });

        rx.await
            .map_err(|_| CliError::expected("VM start channel closed"))?
            .map_err(|e| CliError::expected(format!("VM start failed: {e}")))?;

        info!("VM started");
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn stop(&mut self) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel::<std::result::Result<(), String>>();
        let tx = Mutex::new(Some(tx));
        let vm_addr = self.vm_ptr;

        self.vm_queue.exec_async(move || {
            let handler = RcBlock::new(move |err_ptr: *mut NSError| {
                let result = if err_ptr.is_null() {
                    Ok(())
                } else {
                    let err = unsafe { &*err_ptr };
                    Err(format!("{err}"))
                };
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(result);
                }
            });
            unsafe {
                let vm = &*(vm_addr as *const VZVirtualMachine);
                vm.stopWithCompletionHandler(&handler);
            }
        });

        rx.await
            .map_err(|_| CliError::expected("VM stop channel closed"))?
            .map_err(|e| CliError::expected(format!("VM stop failed: {e}")))?;

        info!("VM stopped");
        Ok(())
    }

    pub async fn wait_for_stop_impl(&self) {
        let vm_addr = self.vm_ptr;
        let queue = self.vm_queue.clone();
        loop {
            let (tx, rx) = tokio::sync::oneshot::channel();
            queue.exec_async(move || {
                let state = unsafe { (*(vm_addr as *const VZVirtualMachine)).state() };
                let _ = tx.send(state);
            });
            if let Ok(state) = rx.await
                && (state == VZVirtualMachineState::Stopped
                    || state == VZVirtualMachineState::Error)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    pub async fn vsock_connect(&self, port: u32) -> Result<OwnedFd> {
        let (tx, rx) = tokio::sync::oneshot::channel::<std::result::Result<i32, String>>();
        let tx = Mutex::new(Some(tx));
        let vm_addr = self.vm_ptr;

        self.vm_queue.exec_async(move || {
            unsafe {
                let vm = &*(vm_addr as *const VZVirtualMachine);
                let devices = vm.socketDevices();
                let Some(device) = devices.firstObject_unchecked() else {
                    if let Some(tx) = tx.lock().unwrap().take() {
                        let _ = tx.send(Err("no vsock device".into()));
                    }
                    return;
                };
                // Downcast VZSocketDevice → VZVirtioSocketDevice
                // Safety: we configured exactly one VZVirtioSocketDeviceConfiguration
                let device_ptr =
                    std::ptr::from_ref::<VZSocketDevice>(device).cast::<VZVirtioSocketDevice>();
                let device = &*device_ptr;

                let handler = RcBlock::new(
                    move |conn_ptr: *mut VZVirtioSocketConnection, err_ptr: *mut NSError| {
                        let result = if err_ptr.is_null() && !conn_ptr.is_null() {
                            let conn = &*conn_ptr;
                            // Dup the fd so it outlives the connection object
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
                        };
                        if let Some(tx) = tx.lock().unwrap().take() {
                            let _ = tx.send(result);
                        }
                    },
                );
                device.connectToPort_completionHandler(port, &handler);
            }
        });

        let fd = rx
            .await
            .map_err(|_| CliError::expected("vsock connect channel closed"))?
            .map_err(|e| CliError::expected(format!("vsock connect failed: {e}")))?;

        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}
