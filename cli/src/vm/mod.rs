#[cfg(target_os = "macos")]
mod apple;
mod config;

use crate::assets::Assets;
use crate::config::Config;
use crate::error::CliError;
use crate::mounts::PreparedMounts;
use std::os::unix::io::OwnedFd;

pub async fn create(
    config: &Config,
    mounts: &PreparedMounts,
) -> Result<(Box<dyn VmHandle>, OwnedFd), CliError> {
    let assets = Assets::init()?;

    let shares: Vec<config::VmShare> = mounts
        .shares
        .iter()
        .map(|s| config::VmShare {
            tag: s.tag.clone(),
            host_path: s.host_path.clone(),
            read_only: s.read_only,
        })
        .collect();

    let vm_config = config::VmConfig {
        cpus: config.cpus,
        memory_bytes: config.memory_mb * 1024 * 1024,
        kernel: assets.kernel,
        initramfs: assets.initramfs,
        kernel_cmdline: {
            let tags: Vec<&str> = shares.iter().map(|s| s.tag.as_str()).collect();
            let mut cmd = format!("console=hvc0 rdinit=/init ezpez.shares={}", tags.join(","));
            if !config.verbose {
                cmd.push_str(" quiet loglevel=3");
            }
            cmd
        },
        shares,
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
