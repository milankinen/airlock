mod de;
pub(crate) mod load_config;
pub(crate) mod presets;
#[cfg(test)]
mod tests;

use config::*;
pub use load_config::load;
pub use smart_config::ByteSize;
use smart_config::{DescribeConfig, DeserializeConfig};

/// Configuration loaded from hierarchical TOML files and validated
/// by smart-config. Runtime-only fields (args, terminal) are set
/// separately after loading.
#[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
pub struct Config {
    /// OCI image to use
    #[config(default_t = "alpine:latest".into())]
    pub image: String,
    /// Number of virtual CPUs
    #[config(default = default_cpus)]
    pub cpus: u32,
    /// Memory size (e.g. "4 GB", "512 MB")
    #[serde(serialize_with = "config::ser_byte_size")]
    #[config(default = default_memory)]
    pub memory: ByteSize,
    /// Network configuration
    #[config(nest)]
    pub network: Network,
    /// Mount points
    #[config(default)]
    pub mounts: Vec<Mount>,
    /// Cache volume (VirtIO block device with ext4)
    #[config(nest)]
    pub cache: Option<Cache>,
}

#[allow(clippy::module_inception)]
pub mod config {
    use std::cmp::{max, min};

    use smart_config::de::WellKnown;
    use smart_config::{DescribeConfig, DeserializeConfig};

    use crate::config::de;

    pub fn default_cpus() -> u32 {
        std::thread::available_parallelism().map_or(2, |n| n.get() as u32)
    }

    pub fn default_memory() -> smart_config::ByteSize {
        use sysinfo::System;
        let sys_bytes = System::new_with_specifics(
            sysinfo::RefreshKind::nothing().with_memory(sysinfo::MemoryRefreshKind::everything()),
        )
        .total_memory();
        let half = sys_bytes / 2;
        let min_bytes = 512 * 1024 * 1024;
        smart_config::ByteSize(min(max(min_bytes, half), sys_bytes))
    }

    #[allow(clippy::trivially_copy_pass_by_ref)] // serde serialize_with requires &T
    pub fn ser_byte_size<S: serde::Serializer>(
        size: &smart_config::ByteSize,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.serialize_str(&size.to_string())
    }

    /// Network configuration
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct Network {
        /// Named network rules. Each rule allows a set of targets and
        /// optionally attaches per-target HTTP middleware. A connection is
        /// allowed if ANY rule allows it.
        #[config(default)]
        pub rules: Vec<NetworkRule>,
    }

    /// A named network rule with allowed targets and optional middleware.
    ///
    /// Target syntax: `host[:port]` — omitted port means all ports.
    /// Both host and port support `*` wildcards.
    ///
    /// Targets matching `localhost` drive VM-side iptables port
    /// forwarding (replacing the old `host_ports` field).
    ///
    /// Targets without middleware get TLS passthrough (no MITM).
    /// Targets with middleware get TLS interception so middleware can
    /// inspect HTTP traffic.
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct NetworkRule {
        /// Rule name (for logging / error messages)
        pub name: String,
        /// Allowed target patterns: `host[:port]`
        #[config(default)]
        pub allow: Vec<String>,
        /// Per-host HTTP middleware (Lua scripts). Key is host pattern,
        /// value is list of scripts. If a host has middleware, TLS is
        /// intercepted.
        #[config(default)]
        pub middleware: std::collections::HashMap<String, Vec<NetworkMiddleware>>,
    }

    impl WellKnown for NetworkRule {
        type Deserializer = de::Nested<NetworkRule>;
        const DE: Self::Deserializer = de::nested();
    }

    /// HTTP middleware script applied to matching targets.
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct NetworkMiddleware {
        /// Inline Lua script
        pub script: String,
    }

    impl WellKnown for NetworkMiddleware {
        type Deserializer = de::Nested<NetworkMiddleware>;
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
        /// What to do when the source path doesn't exist.
        #[config(default_t = MissingAction::Fail)]
        pub missing: MissingAction,
    }

    #[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum MissingAction {
        /// Error out if the source doesn't exist (default).
        Fail,
        /// Skip the mount with a warning.
        Warn,
        /// Skip the mount silently.
        Ignore,
        /// Create the directory and mount it.
        Create,
    }

    impl WellKnown for MissingAction {
        type Deserializer =
            smart_config::de::Serde<{ smart_config::metadata::BasicTypes::STRING.raw() }>;
        const DE: Self::Deserializer = smart_config::de::Serde;
    }

    impl WellKnown for Mount {
        type Deserializer = de::Nested<Mount>;
        const DE: Self::Deserializer = de::nested();
    }

    /// Cache volume configuration — sparse raw disk with ext4
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct Cache {
        /// Disk image size (e.g. "20 GB", "512 MB")
        #[serde(serialize_with = "ser_byte_size")]
        pub size: smart_config::ByteSize,
        /// Container paths to bind-mount from the cache volume
        #[config(default)]
        pub mounts: Vec<String>,
    }
}
