mod load_config;
mod de;

use smart_config::{DescribeConfig, DeserializeConfig};

use config::*;
pub use load_config::load;

/// Configuration loaded from hierarchical TOML files and validated
/// by smart-config. Runtime-only fields (args, terminal) are set
/// separately after loading.
#[derive(Debug, DescribeConfig, DeserializeConfig)]
pub struct Config {
    /// OCI image to use
    #[config(default_t = "alpine:latest".into())]
    pub image: String,
    /// Number of virtual CPUs
    #[config(default = default_cpus)]
    pub cpus: u32,
    /// Memory size in megabytes
    #[config(default = default_memory_mb)]
    pub memory_mb: u64,
    /// Network configuration
    #[config(nest)]
    pub network: Network,
    /// Mount points
    #[config(default)]
    pub mounts: Vec<Mount>,
}

pub mod config {
    use std::cmp::{max, min};
    use smart_config::{DescribeConfig, DeserializeConfig};
    use smart_config::de::WellKnown;
    use crate::config::de;

    pub fn default_cpus() -> u32 {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(2)
    }

    pub fn default_memory_mb() -> u64 {
        use sysinfo::System;
        let sys_mem = System::new_with_specifics(
            sysinfo::RefreshKind::nothing().with_memory(sysinfo::MemoryRefreshKind::everything()),
        ).total_memory() / 1024 / 1024;
        min(max(512, sys_mem / 2), sys_mem)
    }


    /// Network configuration
    #[derive(Debug, DescribeConfig, DeserializeConfig)]
    pub struct Network {
        /// Host ports exposed to the VM
        #[config(default)]
        pub host_ports: Vec<u16>,
    }

    /// Mount point configuration — uses serde (not smart-config derive)
    /// because it appears inside Vec<Mount>.
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct Mount {
        pub source: String,
        pub target: String,
        #[config(default_t = false)]
        pub read_only: bool,
    }

    impl WellKnown for Mount {
        type Deserializer = de::Nested<Mount>;
        const DE: Self::Deserializer = de::nested();
    }
}
