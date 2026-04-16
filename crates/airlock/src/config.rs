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
    pub fn default_memory() -> ByteSize {
        use sysinfo::System;
        let sys_bytes = System::new_with_specifics(
            sysinfo::RefreshKind::nothing().with_memory(sysinfo::MemoryRefreshKind::everything()),
        )
        .total_memory();
        let half = sys_bytes / 2;
        let min_bytes = 512 * 1024 * 1024;
        ByteSize(min(max(min_bytes, half), sys_bytes))
    }

    #[allow(clippy::trivially_copy_pass_by_ref)] // serde serialize_with requires &T
    pub fn ser_byte_size<S: serde::Serializer>(size: &ByteSize, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&size.to_string())
    }

    /// How the OCI image is resolved.
    #[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum Resolution {
        /// Try Docker daemon first, fall back to the registry (default).
        #[default]
        Auto,
        /// Only use the local Docker daemon.
        Docker,
        /// Only pull from the OCI registry.
        Registry,
    }

    impl WellKnown for Resolution {
        type Deserializer =
            smart_config::de::Serde<{ smart_config::metadata::BasicTypes::STRING.raw() }>;
        const DE: Self::Deserializer = smart_config::de::Serde;
    }

    /// OCI image reference — either a plain image name string or a full config object.
    ///
    /// String form:  `image = "alpine:latest"`
    /// Object form:  `[vm.image]\nname = "localhost:5005/alpine:3"\ninsecure = true`
    #[derive(Debug, Clone, serde::Serialize)]
    pub struct ImageRef {
        /// Image name (e.g. `alpine:latest`, `localhost:5005/alpine:3`).
        pub name: String,
        /// Resolution strategy: `auto` (default), `docker`, or `registry`.
        #[serde(default)]
        pub resolution: Resolution,
        /// Allow plain HTTP to the registry (for local or dev registries).
        #[serde(default)]
        pub insecure: bool,
    }

    impl ImageRef {
        pub fn auto(name: impl Into<String>) -> Self {
            Self {
                name: name.into(),
                resolution: Resolution::Auto,
                insecure: false,
            }
        }
    }

    impl std::fmt::Display for ImageRef {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.name)
        }
    }

    impl<'de> serde::Deserialize<'de> for ImageRef {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            #[derive(serde::Deserialize)]
            #[serde(untagged)]
            enum Helper {
                Simple(String),
                Full {
                    name: String,
                    #[serde(default)]
                    resolution: Resolution,
                    #[serde(default)]
                    insecure: bool,
                },
            }
            match Helper::deserialize(d)? {
                Helper::Simple(name) => Ok(ImageRef::auto(name)),
                Helper::Full {
                    name,
                    resolution,
                    insecure,
                } => Ok(ImageRef {
                    name,
                    resolution,
                    insecure,
                }),
            }
        }
    }

    impl WellKnown for ImageRef {
        type Deserializer = smart_config::de::Serde<
            {
                smart_config::metadata::BasicTypes::STRING
                    .or(smart_config::metadata::BasicTypes::OBJECT)
                    .raw()
            },
        >;
        const DE: Self::Deserializer = smart_config::de::Serde;
    }

    /// Virtual machine configurations
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct VirtualMachine {
        /// OCI image to use
        #[config(default_t = ImageRef::auto("alpine:latest"))]
        pub image: ImageRef,
        /// Number of virtual CPUs
        #[config(default = default_cpus)]
        pub cpus: u32,
        /// Memory size (e.g. "4 GB", "512 MB")
        #[serde(serialize_with = "ser_byte_size")]
        #[config(default = default_memory)]
        pub memory: ByteSize,
        /// Enable KVM nested virtualization (Linux only)
        #[config(default)]
        pub kvm: bool,
        /// Apply security hardening to spawned processes (namespace isolation,
        /// no-new-privileges). Disable only for debugging or Docker-in-VM use.
        #[config(default_t = true)]
        pub harden: bool,
        /// Custom kernel image path (overrides the bundled kernel)
        #[config(default)]
        pub kernel: Option<String>,
        /// Custom initramfs path (overrides the bundled initramfs)
        #[config(default)]
        pub initramfs: Option<String>,
    }

    /// Network policy — controls whether connections are allowed or denied
    /// before rules are evaluated.
    #[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum Policy {
        /// Skip rules, allow all connections (default).
        #[default]
        AllowAlways,
        /// Skip rules, deny all connections (including port forwards and sockets).
        DenyAlways,
        /// Allow connections unless explicitly denied by a rule.
        AllowByDefault,
        /// Deny connections unless explicitly allowed by a rule.
        DenyByDefault,
    }

    impl WellKnown for Policy {
        type Deserializer =
            smart_config::de::Serde<{ smart_config::metadata::BasicTypes::STRING.raw() }>;
        const DE: Self::Deserializer = smart_config::de::Serde;
    }

    /// Network configuration
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct Network {
        /// Network policy: `"allow-always"` (default), `"deny-always"`,
        /// `"allow-by-default"`, or `"deny-by-default"`.
        #[config(default)]
        pub policy: Policy,
        /// Named network rules (allow/deny patterns).
        #[config(default)]
        pub rules: BTreeMap<String, NetworkRule>,
        /// Named HTTP middleware scripts.
        #[config(default)]
        pub middleware: BTreeMap<String, MiddlewareRule>,
        /// Port forwarding from guest to host.
        #[config(default)]
        pub ports: BTreeMap<String, PortForward>,
        /// Unix socket forwarding from host to guest.
        #[config(default)]
        pub sockets: BTreeMap<String, SocketForward>,
    }

    /// Forward a host Unix socket into the guest container.
    ///
    /// The `host` field uses `source:target` syntax (host path : guest path),
    /// or a plain path if the same on both sides.
    ///
    /// ```toml
    /// [network.sockets.docker]
    /// host = "~/.docker/run/docker.sock:/var/run/docker.sock"
    /// ```
    #[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct SocketForward {
        /// Enable/disable this socket forward
        #[config(default_t = true)]
        pub enabled: bool,
        /// Socket path mapping: `"source:target"` (host:guest) or plain path
        /// (same on both sides).
        pub host: SocketMapping,
    }

    impl WellKnown for SocketForward {
        type Deserializer = de::Nested<SocketForward>;
        const DE: Self::Deserializer = de::nested();
    }

    /// A socket path mapping: host path to guest path.
    ///
    /// Accepts either a plain path (same on both sides: `"/var/run/docker.sock"`)
    /// or a `"source:target"` string (e.g. `"~/.docker/run/docker.sock:/var/run/docker.sock"`).
    ///
    /// The delimiter is the **last** colon, so paths with colons in early
    /// components are supported (though uncommon for Unix sockets).
    #[derive(Debug, Clone)]
    pub struct SocketMapping {
        pub source: String,
        pub target: String,
    }

    impl serde::Serialize for SocketMapping {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            if self.source == self.target {
                s.serialize_str(&self.source)
            } else {
                s.serialize_str(&format!("{}:{}", self.source, self.target))
            }
        }
    }

    impl<'de> serde::Deserialize<'de> for SocketMapping {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            let s = String::deserialize(d)?;
            // Split on the last colon that is followed by a `/` or `~` (path start).
            // This avoids splitting on colons that are part of directory names.
            if let Some(pos) = s.rfind(':') {
                let target = &s[pos + 1..];
                if target.starts_with('/') || target.starts_with('~') {
                    return Ok(SocketMapping {
                        source: s[..pos].to_string(),
                        target: target.to_string(),
                    });
                }
            }
            Ok(SocketMapping {
                source: s.clone(),
                target: s,
            })
        }
    }

    impl WellKnown for SocketMapping {
        type Deserializer =
            smart_config::de::Serde<{ smart_config::metadata::BasicTypes::STRING.raw() }>;
        const DE: Self::Deserializer = smart_config::de::Serde;
    }

    /// Named port forward group — exposes host TCP ports to the guest.
    #[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct PortForward {
        /// Enable/disable this port forward group
        #[config(default_t = true)]
        pub enabled: bool,
        /// List of ports to forward. Each entry is either a plain port number
        /// (same port on guest and host) or `"source:target"` string.
        /// For `network.ports` (host→guest): source is the host port, target
        /// is the guest port.
        #[config(default)]
        pub host: Vec<PortMapping>,
    }

    impl WellKnown for PortForward {
        type Deserializer = de::Nested<PortForward>;
        const DE: Self::Deserializer = de::nested();
    }

    /// A directional port mapping between two endpoints.
    ///
    /// Accepts either a plain integer (same port both sides: `8080`)
    /// or a `"source:target"` string (e.g. `"9000:8081"`).
    ///
    /// The meaning of source/target depends on context:
    /// - `network.ports` (host→guest): source = host port, target = guest port
    #[derive(Debug, Clone, Copy)]
    pub struct PortMapping {
        pub source: u16,
        pub target: u16,
    }

    impl PortMapping {
        pub fn same(port: u16) -> Self {
            Self {
                source: port,
                target: port,
            }
        }
    }

    impl serde::Serialize for PortMapping {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            if self.source == self.target {
                s.serialize_u16(self.source)
            } else {
                s.serialize_str(&format!("{}:{}", self.source, self.target))
            }
        }
    }

    impl<'de> serde::Deserialize<'de> for PortMapping {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl serde::de::Visitor<'_> for Visitor {
                type Value = PortMapping;

                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("a port number or \"source:target\" string")
                }

                fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<PortMapping, E> {
                    let port = u16::try_from(v).map_err(serde::de::Error::custom)?;
                    Ok(PortMapping::same(port))
                }

                fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<PortMapping, E> {
                    let port = u16::try_from(v).map_err(serde::de::Error::custom)?;
                    Ok(PortMapping::same(port))
                }

                fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<PortMapping, E> {
                    let (source, target) = v
                        .split_once(':')
                        .ok_or_else(|| serde::de::Error::custom("expected \"source:target\""))?;
                    let source: u16 = source.parse().map_err(serde::de::Error::custom)?;
                    let target: u16 = target.parse().map_err(serde::de::Error::custom)?;
                    Ok(PortMapping { source, target })
                }
            }
            d.deserialize_any(Visitor)
        }
    }

    impl WellKnown for PortMapping {
        type Deserializer = smart_config::de::Serde<
            {
                smart_config::metadata::BasicTypes::INTEGER
                    .or(smart_config::metadata::BasicTypes::STRING)
                    .raw()
            },
        >;
        const DE: Self::Deserializer = smart_config::de::Serde;
    }

    /// A named network rule — allow/deny patterns for host:port targets.
    ///
    /// Target syntax: `host[:port]` — omitted port means all ports.
    /// Both host and port support `*` wildcards.
    ///
    /// `deny` is checked first and wins unconditionally. If no rule matches,
    /// the connection follows the network `policy`.
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct NetworkRule {
        /// Enable/disable rule
        #[config(default_t = true)]
        pub enabled: bool,
        /// Hosts/ports to allow.
        #[config(default)]
        pub allow: Vec<String>,
        /// Hosts/ports to deny unconditionally (deny wins over allow).
        #[config(default)]
        pub deny: Vec<String>,
    }

    impl WellKnown for NetworkRule {
        type Deserializer = de::Nested<NetworkRule>;
        const DE: Self::Deserializer = de::nested();
    }

    /// HTTP middleware script with target patterns.
    ///
    /// Middleware is applied to allowed connections whose host:port matches
    /// any entry in `target`. Triggers TLS interception for HTTPS traffic.
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct MiddlewareRule {
        /// Enable/disable this middleware
        #[config(default_t = true)]
        pub enabled: bool,
        /// Host:port patterns where this middleware applies (same syntax as
        /// rule allow/deny).
        #[config(default)]
        pub target: Vec<String>,
        /// Variables exposed to the script as the `env` global table.
        /// Values are subst templates (e.g. `"${HOST_VAR}"`) expanded from
        /// the host environment. Any template referencing an undefined host
        /// variable resolves to nil in the script.
        #[config(default)]
        pub env: BTreeMap<String, String>,
        /// Inline Lua script
        pub script: String,
    }

    impl WellKnown for MiddlewareRule {
        type Deserializer = de::Nested<MiddlewareRule>;
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
        /// Unix permissions for created dirs/files (octal string, e.g. "755").
        /// Default: "755" for directories, "644" for files.
        #[config(default)]
        pub create_mode: Option<String>,
        /// Initial content written when `missing = "create-file"` creates the file.
        #[config(default)]
        pub file_content: Option<String>,
    }

    #[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum MissingAction {
        /// Error out if the source doesn't exist (default).
        Fail,
        /// Skip the mount with a warning.
        Warn,
        /// Skip the mount silently.
        Ignore,
        /// Create the directory and mount it.
        CreateDir,
        /// Create the file (with optional content) and mount it.
        CreateFile,
    }

    /// VM disk image configuration — sparse raw disk with ext4
    #[derive(Debug, serde::Serialize, DescribeConfig, DeserializeConfig)]
    pub struct Disk {
        /// Disk image size (e.g. "20 GB", "512 MB"). Default 10 GB.
        #[serde(serialize_with = "ser_byte_size")]
        #[config(default_t = ByteSize(10 * 1024 * 1024 * 1024))]
        pub size: ByteSize,
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
