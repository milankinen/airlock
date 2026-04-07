/// Parameters passed from the host CLI that influence guest VM initialization.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct InitConfig {
    /// Wall-clock epoch (seconds since Unix epoch) to set as the guest system
    /// clock. VMs don't have an RTC, so the host provides the current time.
    pub epoch: u64,
    /// Host TCP ports whose traffic should be redirected through the network
    /// proxy (iptables REDIRECT) so the sandbox can intercept localhost traffic.
    pub host_ports: Vec<u16>,
}

#[cfg(target_os = "linux")]
mod linux;

/// Bootstrap the guest VM environment: clock, mounts, networking, rootfs.
#[cfg(target_os = "linux")]
pub fn setup(config: &InitConfig) -> anyhow::Result<()> {
    linux::setup(config)
}

/// Stub for non-Linux hosts — the supervisor binary is cross-compiled for
/// Linux but the rest of the workspace is built on macOS for dev tooling.
#[cfg(not(target_os = "linux"))]
pub fn setup(_config: &InitConfig) -> anyhow::Result<()> {
    unimplemented!("supervisor only runs inside the Linux VM");
}
