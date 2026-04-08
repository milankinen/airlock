//! Hierarchical TOML configuration with preset support.
//!
//! Configuration is loaded from up to four files (global, home, project,
//! local), merged with deep-merge semantics, and validated by `smart-config`.

mod de;
pub(crate) mod load_config;
pub(crate) mod presets;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use config::*;
pub use load_config::load;
use smart_config::{DescribeConfig, DeserializeConfig};

/// Configuration loaded from hierarchical TOML files and validated
/// by smart-config. Runtime-only fields (args, terminal) are set
/// separately after loading.
#[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
pub struct Config {
    /// Virtual machine configuration
    #[config(nest)]
    pub vm: VirtualMachine,
    /// Network configuration
    #[config(nest)]
    pub network: Network,
    /// Mount points
    #[config(default)]
    pub mounts: BTreeMap<String, Mount>,
    /// Cache volume (VirtIO block device with ext4)
    #[config(nest)]
    pub disk: Disk,
    /// Environment variables injected into the container.
    /// Values support `${VAR}` substitution from the host environment.
    #[config(default)]
    pub env: BTreeMap<String, String>,
}

#[allow(clippy::module_inception)]
pub mod config {
    use std::cmp::{max, min};
    use std::collections::BTreeMap;

    use smart_config::de::WellKnown;
    use smart_config::{ByteSize, DescribeConfig, DeserializeConfig};

    use crate::config::de;

    /// Default to all available host CPUs.
    pub fn default_cpus() -> u32 {
        std::thread::available_parallelism().map_or(2, |n| n.get() as u32)
    }

    /// Default to half of total system RAM, clamped to [512 MB, total].
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

    /// Virtual machine configurations
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct VirtualMachine {
        /// OCI image to use
        #[config(default_t = "alpine:latest".into())]
        pub image: String,
        /// Number of virtual CPUs
        #[config(default = default_cpus)]
        pub cpus: u32,
        /// Memory size (e.g. "4 GB", "512 MB")
        #[serde(serialize_with = "ser_byte_size")]
        #[config(default = default_memory)]
        pub memory: ByteSize,
        /// Enable nested virtualization (Linux only)
        #[config(default)]
        pub nested_virtualization: bool,
        /// Custom kernel image path (overrides the bundled kernel)
        #[config(default)]
        pub kernel: Option<String>,
        /// Custom initramfs path (overrides the bundled initramfs)
        #[config(default)]
        pub initramfs: Option<String>,
    }

    /// Network configuration
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct Network {
        /// Named network rules. Each rule allows a set of targets and
        /// optionally attaches per-target HTTP middleware. A connection is
        /// allowed if ANY rule allows it.
        #[config(default)]
        pub rules: BTreeMap<String, NetworkRule>,
        /// Unix socket forwarding from host to guest.
        #[config(default)]
        pub sockets: BTreeMap<String, SocketForward>,
    }

    /// Forward a host Unix socket into the guest container.
    #[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct SocketForward {
        /// Enable/disable this socket forward
        #[config(default_t = true)]
        pub enabled: bool,
        /// Host socket path (e.g., "/var/run/docker.sock")
        pub host: String,
        /// Guest socket path (e.g., "/var/run/docker.sock")
        pub guest: String,
    }

    impl WellKnown for SocketForward {
        type Deserializer = de::Nested<SocketForward>;
        const DE: Self::Deserializer = de::nested();
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
        /// Enable/disable rule
        #[config(default_t = true)]
        pub enabled: bool,
        /// Allowed target patterns: `host[:port]`
        #[config(default)]
        pub allow: Vec<String>,
        /// Middleware to apply for the allowed hosts. If any middleware is
        /// set, then also TLS connections will be encrypted and intercepted.
        /// If client is using certification pinning, it will break, otherwise
        /// interception should be transparent to the client.
        #[config(default)]
        pub middleware: Vec<NetworkMiddleware>,
    }

    impl WellKnown for NetworkRule {
        type Deserializer = de::Nested<NetworkRule>;
        const DE: Self::Deserializer = de::nested();
    }

    /// HTTP middleware script applied to matching targets.
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct NetworkMiddleware {
        /// Variables exposed to the script as the `env` global table.
        /// Values are subst templates (e.g. `"${HOST_VAR}"`) expanded from
        /// the host environment. Any template referencing an undefined host
        /// variable resolves to nil in the script.
        #[config(default)]
        pub env: BTreeMap<String, String>,
        /// Inline Lua script
        pub script: String,
    }

    impl WellKnown for NetworkMiddleware {
        type Deserializer = de::Nested<NetworkMiddleware>;
        const DE: Self::Deserializer = de::nested();
    }

    /// Mount point configuration.
    #[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct Mount {
        /// Enable/disable mount
        #[config(default_t = true)]
        pub enabled: bool,
        /// Source path in the host
        pub source: String,
        /// Target path in the VM container
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

    /// VM disk image configuration — sparse raw disk with ext4
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct Disk {
        /// Disk image size (e.g. "20 GB", "512 MB"). Default 10 GB.
        #[serde(serialize_with = "ser_byte_size")]
        #[config(default_t = smart_config::ByteSize(10 * 1024 * 1024 * 1024))]
        pub size: smart_config::ByteSize,
        /// Container paths to bind-mount from the cache volume
        #[config(default)]
        pub cache: BTreeMap<String, CacheMount>,
    }

    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct CacheMount {
        /// Enable/disable mount
        #[config(default_t = true)]
        pub enabled: bool,
        /// One or more container paths to back with persistent cache storage
        pub paths: Vec<String>,
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

    impl WellKnown for CacheMount {
        type Deserializer = de::Nested<CacheMount>;
        const DE: Self::Deserializer = de::nested();
    }
}
