mod de;
mod load_config;

use config::*;
pub use load_config::load;
use smart_config::{DescribeConfig, DeserializeConfig};

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

#[allow(clippy::module_inception)]
pub mod config {
    use std::cmp::{max, min};

    use smart_config::de::WellKnown;
    use smart_config::{DescribeConfig, DeserializeConfig, Serde};

    use crate::config::de;

    pub fn default_cpus() -> u32 {
        std::thread::available_parallelism().map_or(2, |n| n.get() as u32)
    }

    pub fn default_memory_mb() -> u64 {
        use sysinfo::System;
        let sys_mem = System::new_with_specifics(
            sysinfo::RefreshKind::nothing().with_memory(sysinfo::MemoryRefreshKind::everything()),
        )
        .total_memory()
            / 1024
            / 1024;
        min(max(512, sys_mem / 2), sys_mem)
    }

    /// Network configuration
    #[derive(Debug, DescribeConfig, DeserializeConfig)]
    pub struct Network {
        /// Host ports exposed to the VM
        #[config(default)]
        pub host_ports: Vec<u16>,
        /// Default network policy: "allow" or "deny"
        #[config(with = Serde![str])]
        #[config(default_t = NetworkMode::Deny)]
        pub default_mode: NetworkMode,
        /// Network filtering rules (Lua scripts)
        #[config(default)]
        pub rules: Vec<NetworkRule>,
        /// Hosts whose TLS traffic should NOT be intercepted (cert pinning support).
        /// Supports glob patterns like "*.example.com".
        #[config(default)]
        pub allowed_hosts_tls: Vec<String>,
    }

    #[derive(Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum NetworkMode {
        Allow,
        Deny,
    }

    /// Network filtering rule with inline Lua script
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct NetworkRule {
        /// Rule name (for error messages)
        pub name: String,
        /// Rule type: "tcp_connect" or "http_request"
        #[config(with = Serde![str])]
        pub r#type: NetworkRuleType,
        /// Required env vars: name → description
        #[config(default)]
        pub env: std::collections::HashMap<String, String>,
        /// Inline Lua script
        pub script: String,
    }

    #[derive(Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum NetworkRuleType {
        TcpConnect,
        HttpRequest,
    }

    impl WellKnown for NetworkRule {
        type Deserializer = de::Nested<NetworkRule>;
        const DE: Self::Deserializer = de::nested();
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
