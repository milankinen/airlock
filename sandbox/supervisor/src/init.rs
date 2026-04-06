#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct InitConfig {
    pub epoch: u64,
    pub host_ports: Vec<u16>,
}

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub fn setup(config: &InitConfig) -> anyhow::Result<()> {
    linux::setup(config)
}

#[cfg(not(target_os = "linux"))]
pub fn setup(_config: &InitConfig) -> anyhow::Result<()> {
    unimplemented!("supervisor only runs inside the Linux VM");
}
