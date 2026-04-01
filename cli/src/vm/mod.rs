#[cfg(target_os = "macos")]
mod apple;
mod config;

use crate::config::Config;
use crate::error::CliError;
use std::os::unix::io::OwnedFd;

pub async fn create(config: &Config) -> Result<(Box<dyn VmHandle>, OwnedFd), CliError> {
    let assets = crate::assets::extract_assets()?;

    let vm_config = config::VmConfig {
        cpus: config.cpus,
        memory_bytes: config.memory_mb * 1024 * 1024,
        kernel: assets.kernel,
        initramfs: assets.initramfs,
        kernel_cmdline: if config.verbose {
            "console=hvc0 rdinit=/init".to_string()
        } else {
            "console=hvc0 rdinit=/init quiet loglevel=3".to_string()
        },
        bundle_path: Some(config.bundle_path.clone()),
    };

    #[cfg(target_os = "macos")]
    {
        let mut backend = apple::AppleVmBackend::new(vm_config)?;
        backend.start().await?;

        let vsock_fd = {
            let mut attempts = 0;
            loop {
                match backend.vsock_connect(ezpez_protocol::SUPERVISOR_PORT).await {
                    Ok(fd) => break fd,
                    Err(e) => {
                        attempts += 1;
                        if attempts >= 30 {
                            return Err(CliError::expected(format!(
                                "supervisor not reachable after {attempts} attempts: {e}"
                            )));
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        };

        Ok((Box::new(backend), vsock_fd))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = vm_config;
        Err(CliError::expected("only macOS is supported currently"))
    }
}

#[allow(dead_code)]
pub trait VmHandle {
    fn wait_for_stop(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>>;
}

#[cfg(target_os = "macos")]
impl VmHandle for apple::AppleVmBackend {
    fn wait_for_stop(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>> {
        Box::pin(apple::AppleVmBackend::wait_for_stop_impl(self))
    }
}
