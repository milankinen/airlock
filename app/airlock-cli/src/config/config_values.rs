//! Config schema.
//!
//! Defines all config sections and their values and defaults. Parses a
//! merged config document and checks it for problems, including problems
//! that the schema alone cannot express.

use std::cmp::{max, min};
use std::collections::BTreeMap;

use smart_config::de::WellKnown;
use smart_config::{ByteSize, DescribeConfig, DeserializeConfig};

use crate::config::de;
use crate::config::de::format_error;
use crate::network::rules::parse_pattern;
use crate::services::ServiceId;

/// Configuration from the layered config files, validated by
/// smart-config.
#[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
pub struct ConfigValues {
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
    /// Environment variables for the container.
    /// Values support `${VAR}` substitution from the host environment.
    /// An entry can be a plain string or `{ value = "...", mask = true }`.
    /// The guest gets a masked entry as a stable surrogate of the same
    /// length (see [`EnvVar`]).
    #[config(default)]
    pub env: BTreeMap<String, EnvVar>,
    /// Sidecar processes. They start during the boot, before other
    /// processes.
    #[config(default)]
    pub daemons: BTreeMap<String, Daemon>,
    /// Subdirectories of the project mount to hide from the sandbox. An
    /// empty directory is bind-mounted over each of them. Use it to hide
    /// parts of a monorepo from AI agents in the VM.
    #[config(default)]
    pub mask: BTreeMap<String, Mask>,
    /// Clipboard bridge between the sandbox and the host clipboard.
    /// Both directions are off by default, because each one is an
    /// intentional hole in the sandbox (see `[clipboard]` in the manual).
    #[config(nest)]
    pub clipboard: Clipboard,
}

/// Default number of CPUs: all available host CPUs.
pub fn default_cpus() -> u32 {
    std::thread::available_parallelism().map_or(2, |n| n.get() as u32)
}

/// Default memory size: half of the total system RAM, clamped to
/// [512 MB, total].
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

/// Serialize a byte size as text (for example `"4 GB"`).
#[allow(clippy::trivially_copy_pass_by_ref)] // serde serialize_with requires &T
pub fn ser_byte_size<S: serde::Serializer>(size: &ByteSize, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&size.to_string())
}

/// Source of the OCI image.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resolution {
    /// Try local Docker images, then Podman, then the registry (default).
    #[default]
    Auto,
    /// Only use local Docker images.
    Docker,
    /// Only use local Podman images.
    Podman,
    /// Only pull from the OCI registry.
    Registry,
}

impl WellKnown for Resolution {
    type Deserializer =
        smart_config::de::Serde<{ smart_config::metadata::BasicTypes::STRING.raw() }>;
    const DE: Self::Deserializer = smart_config::de::Serde;
}

/// When to resolve the configured image reference again against its
/// source.
///
/// This is independent of [`Resolution`]. [`Resolution`] selects *where*
/// an image comes from. This policy selects *how often* airlock asks the
/// source. It has no effect for digest-pinned references
/// (`repo@sha256:…`). They name one immutable image, thus airlock never
/// resolves them again to find changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullPolicy {
    /// Use the locally cached image if there is one with the configured
    /// name. Do not contact the source (default).
    #[default]
    IfNotPresent,
    /// Resolve the reference to a digest again at each start. Use the
    /// cached image only if that digest is still the same.
    IfChanged,
}

impl WellKnown for PullPolicy {
    type Deserializer =
        smart_config::de::Serde<{ smart_config::metadata::BasicTypes::STRING.raw() }>;
    const DE: Self::Deserializer = smart_config::de::Serde;
}

/// OCI image reference: a plain image name string or a full config object.
///
/// String form:  `image = "alpine:latest"`
/// Object form:  `[vm.image]\nname = "localhost:5005/alpine:3"\ninsecure = true`
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageRef {
    /// Image name (for example `alpine:latest`, `localhost:5005/alpine:3`).
    /// A digest can be pinned with `@sha256:…`, also together with a tag
    /// (`alpine:3.20@sha256:…`), the same as Docker tools accept.
    pub name: String,
    /// Resolution strategy: `auto` (default), `docker`, `podman`, or
    /// `registry`.
    #[serde(default)]
    pub resolution: Resolution,
    /// Allow plain HTTP to the registry (for local or dev registries).
    #[serde(default)]
    pub insecure: bool,
    /// When to resolve the reference again: `if-not-present` (default) or
    /// `if-changed`.
    #[serde(default, rename = "pull-policy")]
    pub pull_policy: PullPolicy,
}

impl ImageRef {
    /// Make a reference to `name` with default settings.
    pub fn auto(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            resolution: Resolution::Auto,
            insecure: false,
            pull_policy: PullPolicy::IfNotPresent,
        }
    }

    /// Get the digest pinned in the reference (`@sha256:…`), if any.
    ///
    /// A pinned reference names exactly one immutable image. Callers use
    /// the digest to skip the tag→digest change detection. They also use it
    /// to check that the image from a source is the requested image.
    pub fn pinned_digest(&self) -> Option<&str> {
        let (name, digest) = self.name.rsplit_once('@')?;
        // Do not accept `@` in a different position. A digest is
        // `<algorithm>:<hex>`, and nothing can follow it. The name must also
        // stay: without the digest, there must be a name to query a source
        // with.
        let (algorithm, hex) = digest.split_once(':')?;
        let valid = !name.is_empty()
            && !algorithm.is_empty()
            && hex.len() >= 32
            && hex.chars().all(|c| c.is_ascii_hexdigit())
            && algorithm
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.'));
        valid.then_some(digest)
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
                // The untagged helper ignores unknown keys. Without the
                // alias, the snake_case spelling would have no effect and no
                // error. All other config sections here use snake_case.
                #[serde(default, rename = "pull-policy", alias = "pull_policy")]
                pull_policy: PullPolicy,
            },
        }
        match Helper::deserialize(d)? {
            Helper::Simple(name) => Ok(ImageRef::auto(name)),
            Helper::Full {
                name,
                resolution,
                insecure,
                pull_policy,
            } => Ok(ImageRef {
                name,
                resolution,
                insecure,
                pull_policy,
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

/// One `[env]` entry: a plain string or a full config object.
///
/// String form:  `TOKEN = "${TOKEN}"`
/// Object form:  `TOKEN = { value = "${TOKEN}", mask = true, optional = true }`
///
/// With `mask = true`, the guest sees a stable alphanumeric surrogate of
/// the same length instead of the real value. The surrogate comes from the
/// variable name and the length, never from the value. The host can still
/// put the real value into outbound HTTP headers through the `inject` list
/// of a network rule.
///
/// With `optional = true`, an entry whose template reads an undefined
/// variable is left out of the guest, and `inject` lists skip it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EnvVar {
    /// Value template. Supports `${VAR}` substitution from the host.
    pub value: String,
    /// Replace the value with a same-length surrogate inside the guest.
    pub mask: bool,
    /// Leave the entry out if the template reads an undefined variable.
    /// Not serialized when false, thus configs without it serialize as
    /// before.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

impl EnvVar {
    /// A plain, unmasked entry.
    pub fn plain(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            mask: false,
            optional: false,
        }
    }
}

impl<'de> serde::Deserialize<'de> for EnvVar {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        // `deny_unknown_fields` makes `{ value = "…", masked = true }` an
        // error. Without it, the entry would silently get `mask = false`
        // and the real value would go into the guest. The match on the raw
        // value (not an untagged enum) keeps the error message that names
        // the unknown key. An untagged enum gives only the generic serde
        // message "did not match any variant".
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Full {
            value: String,
            #[serde(default)]
            mask: bool,
            #[serde(default)]
            optional: bool,
        }
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::String(value) => Ok(EnvVar::plain(value)),
            table @ serde_json::Value::Object(_) => {
                let Full {
                    value,
                    mask,
                    optional,
                } = serde_json::from_value(table).map_err(D::Error::custom)?;
                Ok(EnvVar {
                    value,
                    mask,
                    optional,
                })
            }
            _ => Err(D::Error::custom(
                "expected a string or a table `{ value = \"...\", mask = true }`",
            )),
        }
    }
}

impl WellKnown for EnvVar {
    type Deserializer = smart_config::de::Serde<
        {
            smart_config::metadata::BasicTypes::STRING
                .or(smart_config::metadata::BasicTypes::OBJECT)
                .raw()
        },
    >;
    const DE: Self::Deserializer = smart_config::de::Serde;
}

/// Virtual machine configuration.
#[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
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
    /// Enable nested virtualization and expose KVM in the guest
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

/// Network policy. It controls if connections are allowed or denied
/// before the rules are evaluated.
///
/// It is also the value type of `airlock start --network <POLICY>`. The
/// `clap` value names are the same as the kebab-case form in the file.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    /// Skip rules, allow all connections.
    AllowAlways,
    /// Skip rules, deny all connections (including port forwards and sockets).
    DenyAlways,
    /// Allow connections unless explicitly denied by a rule.
    AllowByDefault,
    /// Deny connections unless explicitly allowed by a rule (default).
    #[default]
    DenyByDefault,
}

impl Policy {
    /// Kebab-case name as written in `airlock.toml` and on the CLI.
    pub fn label(self) -> &'static str {
        match self {
            Policy::AllowAlways => "allow-always",
            Policy::DenyAlways => "deny-always",
            Policy::AllowByDefault => "allow-by-default",
            Policy::DenyByDefault => "deny-by-default",
        }
    }
}

impl WellKnown for Policy {
    type Deserializer =
        smart_config::de::Serde<{ smart_config::metadata::BasicTypes::STRING.raw() }>;
    const DE: Self::Deserializer = smart_config::de::Serde;
}

/// Network configuration.
#[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
pub struct Network {
    /// Network policy: `"allow-always"`, `"deny-always"`,
    /// `"allow-by-default"`, or `"deny-by-default"` (default).
    #[config(default)]
    pub policy: Policy,
    /// Named network rules (allow/deny patterns).
    #[config(default)]
    pub rules: BTreeMap<String, NetworkRule>,
    /// Named HTTP middleware scripts.
    #[config(default)]
    pub middleware: BTreeMap<String, MiddlewareRule>,
    /// Port forwarding between guest and host, in both directions.
    #[config(default)]
    pub ports: BTreeMap<String, PortForward>,
    /// Unix socket forwarding from host to guest.
    #[config(default)]
    pub sockets: BTreeMap<String, SocketForward>,
    /// Network services by name (`anthropic`, `openai`). `true` lets
    /// airlock run the sign-in of the agent and keep its real tokens on the
    /// host (see [`crate::services`]). All are off by default. An empty map
    /// is not serialized, thus configs without services serialize as
    /// before.
    #[config(default)]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, bool>,
}

/// Forward a host Unix socket into the guest container.
///
/// The `host` field uses `source:target` syntax (host path : guest path),
/// or a plain path if both sides are the same.
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
    /// Socket path mapping: `"source:target"` (host:guest) or a plain path
    /// (the same on both sides).
    pub host: SocketMapping,
}

impl WellKnown for SocketForward {
    type Deserializer = de::Nested<SocketForward>;
    const DE: Self::Deserializer = de::nested();
}

/// A socket path mapping: host path to guest path.
///
/// Accepts a plain path (the same on both sides: `"/var/run/docker.sock"`)
/// or a `"source:target"` string (for example
/// `"~/.docker/run/docker.sock:/var/run/docker.sock"`).
///
/// The delimiter is the **last** colon that a path start (`/` or `~`)
/// follows. Thus paths with colons in earlier components are supported
/// (but they are not usual for Unix sockets).
#[derive(Debug, Clone)]
pub struct SocketMapping {
    /// Socket path on the host.
    pub source: String,
    /// Socket path in the guest.
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
        // Split at the last colon if a `/` or `~` (path start) follows it.
        // Thus colons in directory names do not split the path.
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

/// Named port forward group. Forwards TCP ports between host and guest in
/// one of the two directions.
#[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
pub struct PortForward {
    /// Enable/disable this port forward group
    #[config(default_t = true)]
    pub enabled: bool,
    /// Guest → host forwards. Each entry is a plain port number (the same
    /// port on both sides) or a `"host:guest"` string. A guest process that
    /// connects to `localhost:<guest_port>` reaches the listed host port.
    #[config(default)]
    pub host: Vec<PortMapping>,
    /// Host → guest forwards. Each entry is a plain port number (the same
    /// port on both sides) or a `"host:guest"` string. A host process that
    /// connects to `127.0.0.1:<host_port>` reaches the listed guest port.
    /// Traffic from the host skips all rules, policy and middleware,
    /// because the host is trusted. Listeners bind only on `127.0.0.1`.
    #[config(default)]
    pub guest: Vec<PortMapping>,
}

impl WellKnown for PortForward {
    type Deserializer = de::Nested<PortForward>;
    const DE: Self::Deserializer = de::nested();
}

/// A port mapping between a host port and a guest port.
///
/// Accepts a plain integer (the same port on both sides: `8080`) or a
/// `"host:guest"` string (for example `"9000:8081"`).
///
/// The left side of the colon is always the host port, and the right side
/// is always the guest port, in both lists. The list sets the direction of
/// the forward:
///  - `[network.ports.<name>].host` forwards guest → host (the guest side
///    opens the connection to the host port).
///  - `[network.ports.<name>].guest` forwards host → guest (the host side
///    opens the connection to the guest port).
#[derive(Debug, Clone, Copy)]
pub struct PortMapping {
    /// Port on the host.
    pub host: u16,
    /// Port in the guest.
    pub guest: u16,
}

impl PortMapping {
    /// Make a mapping with the same port on both sides.
    pub fn same(port: u16) -> Self {
        Self {
            host: port,
            guest: port,
        }
    }
}

impl serde::Serialize for PortMapping {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.host == self.guest {
            s.serialize_u16(self.host)
        } else {
            s.serialize_str(&format!("{}:{}", self.host, self.guest))
        }
    }
}

impl<'de> serde::Deserialize<'de> for PortMapping {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = PortMapping;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a port number or \"host:guest\" string")
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
                let (host, guest) = v
                    .split_once(':')
                    .ok_or_else(|| serde::de::Error::custom("expected \"host:guest\""))?;
                let host: u16 = host.parse().map_err(serde::de::Error::custom)?;
                let guest: u16 = guest.parse().map_err(serde::de::Error::custom)?;
                Ok(PortMapping { host, guest })
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

/// A named network rule: allow and deny patterns for host:port targets.
///
/// Target syntax: `host[:port]`. Without a port, all ports match. Host and
/// port both support `*` wildcards. A port that is not a number or `*`
/// (`:8O80`, `:https`, a trailing space) is a configuration error, never a
/// wildcard.
///
/// `deny` is checked first and always wins. If no rule matches, the
/// connection follows the network `policy`.
///
/// With `passthrough = true`, `allow` targets skip all TLS/HTTP
/// interception, and the connection is a pure TCP relay. This is necessary
/// for protocols that are not HTTP and whose first client bytes cannot be
/// sniffed. For example, the 8-byte SSLRequest of Postgres would deadlock
/// the HTTP detector. Middleware on the same target is not compatible.
/// Airlock reports the conflict at startup.
#[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
pub struct NetworkRule {
    /// Enable or disable the rule
    #[config(default_t = true)]
    pub enabled: bool,
    /// Hosts/ports to allow.
    #[config(default)]
    pub allow: Vec<String>,
    /// Hosts/ports to deny unconditionally (deny wins over allow).
    #[config(default)]
    pub deny: Vec<String>,
    /// If true, allowed targets are relayed as plain TCP without TLS or
    /// HTTP interception. Middleware on the same target is an error.
    #[config(default)]
    pub passthrough: bool,
    /// Names of masked `[env]` variables. For the allow targets of this
    /// rule, their real value goes into HTTP request headers, and is masked
    /// again in response headers. Each name must be in `[env]` with
    /// `mask = true`. Not compatible with `passthrough`.
    #[config(default)]
    pub inject: Vec<String>,
}

impl WellKnown for NetworkRule {
    type Deserializer = de::Nested<NetworkRule>;
    const DE: Self::Deserializer = de::nested();
}

/// HTTP middleware script with target patterns.
///
/// Middleware applies to allowed connections whose host:port matches an
/// entry in `target`. It causes TLS interception for HTTPS traffic.
#[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
pub struct MiddlewareRule {
    /// Enable or disable this middleware
    #[config(default_t = true)]
    pub enabled: bool,
    /// Host:port patterns where this middleware applies (the same syntax as
    /// rule allow/deny).
    #[config(default)]
    pub target: Vec<String>,
    /// Variables for the script, in the `env` global table. Values are
    /// subst templates (for example `"${HOST_VAR}"`), expanded from the host
    /// environment. A template that reads an undefined host variable is nil
    /// in the script.
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
    /// Enable or disable the mount
    #[config(default_t = true)]
    pub enabled: bool,
    /// Source path in the host
    pub source: String,
    /// Target path in the VM container
    pub target: String,
    /// Mount as read-only
    #[config(default_t = false)]
    pub read_only: bool,
    /// What to do if the source path does not exist.
    #[config(default_t = MissingAction::Fail)]
    pub missing: MissingAction,
    /// Unix permissions for created directories and files (octal string,
    /// for example "755"). Default: "755" for directories, "644" for files.
    #[config(default)]
    pub create_mode: Option<String>,
    /// Initial content of the file that `missing = "create-file"` creates.
    #[config(default)]
    pub file_content: Option<String>,
}

/// What to do if the source path of a mount does not exist.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MissingAction {
    /// Stop with an error (default).
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

/// VM disk image configuration: sparse raw disk with ext4.
#[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
pub struct Disk {
    /// Disk image size (for example "20 GB", "512 MB"). Default 10 GB.
    #[serde(serialize_with = "ser_byte_size")]
    #[config(default_t = ByteSize(10 * 1024 * 1024 * 1024))]
    pub size: ByteSize,
    /// Container paths to bind-mount from the cache volume
    #[config(default)]
    pub cache: BTreeMap<String, CacheMount>,
}

/// Container paths on the persistent cache volume (`[disk.cache.<name>]`).
#[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
pub struct CacheMount {
    /// Enable or disable the mount
    #[config(default_t = true)]
    pub enabled: bool,
    /// One or more container paths to back with persistent cache storage
    pub paths: Vec<String>,
}

/// Clipboard bridge configuration (`[clipboard]`).
///
/// Each direction is granted separately, and both are off by default. For
/// a disabled direction, the host never gives the capability to the guest.
/// Thus a compromised sandbox has nothing to call. The booleans grant a
/// capability. They are not a policy check in the guest.
#[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
pub struct Clipboard {
    /// Let the sandbox write to the host clipboard. The risk is the content,
    /// not the size: text that is copied from the sandbox can later be
    /// pasted into a shell.
    #[config(default_t = false)]
    pub copy: bool,
    /// Maximum size of one guest → host transfer (for example "2 MB"). The
    /// host enforces it. The guest daemon also enforces it, thus an
    /// oversized write is never buffered. It limits memory use, not what the
    /// content can do.
    #[serde(serialize_with = "ser_byte_size")]
    #[config(default_t = ByteSize(1024 * 1024))]
    pub copy_limit: ByteSize,
    /// Let the sandbox read the host clipboard. The guest *starts* the read
    /// without user interaction. Thus code in the sandbox can get the last
    /// copied text, for example passwords or tokens. Enable it only if
    /// necessary.
    #[config(default_t = false)]
    pub paste: bool,
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

/// Restart policy for a daemon process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Restart on any exit until `max_restarts` is reached (default).
    #[default]
    Always,
    /// Restart only on a non-zero exit. Stop the loop on a clean exit.
    OnFailure,
}

impl WellKnown for RestartPolicy {
    type Deserializer =
        smart_config::de::Serde<{ smart_config::metadata::BasicTypes::STRING.raw() }>;
    const DE: Self::Deserializer = smart_config::de::Serde;
}

/// Unix signal that asks a daemon to stop gracefully.
///
/// Accepts the canonical name (for example `"SIGTERM"`). An unknown name
/// is a config parse error. Signal numbers are not accepted, because they
/// are not the same on all platforms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Signal {
    /// `SIGTERM` (default).
    #[default]
    Term,
    /// `SIGINT`.
    Int,
    /// `SIGHUP`.
    Hup,
    /// `SIGQUIT`.
    Quit,
    /// `SIGUSR1`.
    Usr1,
    /// `SIGUSR2`.
    Usr2,
    /// `SIGKILL`.
    Kill,
}

impl Signal {
    /// Linux signal number. For these signals, it is the same on all
    /// architectures.
    pub fn as_number(self) -> i32 {
        match self {
            Signal::Hup => 1,
            Signal::Int => 2,
            Signal::Quit => 3,
            Signal::Kill => 9,
            Signal::Usr1 => 10,
            Signal::Term => 15,
            Signal::Usr2 => 12,
        }
    }
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Signal::Term => "SIGTERM",
            Signal::Int => "SIGINT",
            Signal::Hup => "SIGHUP",
            Signal::Quit => "SIGQUIT",
            Signal::Usr1 => "SIGUSR1",
            Signal::Usr2 => "SIGUSR2",
            Signal::Kill => "SIGKILL",
        };
        f.write_str(s)
    }
}

impl serde::Serialize for Signal {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Signal {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "SIGTERM" => Ok(Signal::Term),
            "SIGINT" => Ok(Signal::Int),
            "SIGHUP" => Ok(Signal::Hup),
            "SIGQUIT" => Ok(Signal::Quit),
            "SIGUSR1" => Ok(Signal::Usr1),
            "SIGUSR2" => Ok(Signal::Usr2),
            "SIGKILL" => Ok(Signal::Kill),
            other => Err(serde::de::Error::custom(format!(
                "unknown signal '{other}' — expected one of SIGTERM, SIGINT, SIGHUP, \
                 SIGQUIT, SIGUSR1, SIGUSR2, SIGKILL"
            ))),
        }
    }
}

impl WellKnown for Signal {
    type Deserializer =
        smart_config::de::Serde<{ smart_config::metadata::BasicTypes::STRING.raw() }>;
    const DE: Self::Deserializer = smart_config::de::Serde;
}

/// Sidecar process declared under `[daemons.<name>]`.
///
/// Daemons start during the boot, before other processes. They restart
/// as `restart` and `max_restarts` specify. At sandbox shutdown, they get
/// `signal`, then SIGKILL after `timeout` seconds.
#[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
pub struct Daemon {
    /// Enable or disable this daemon
    #[config(default_t = true)]
    pub enabled: bool,
    /// Argv for the daemon process
    pub command: Vec<String>,
    /// Working directory inside the container (default "/")
    #[config(default_t = String::from("/"))]
    pub cwd: String,
    /// Signal used for graceful shutdown (default SIGTERM).
    #[config(default)]
    pub signal: Signal,
    /// Time in seconds between `signal` and SIGKILL. `0` means wait
    /// forever.
    #[config(default_t = 10)]
    pub timeout: u32,
    /// Restart policy: `"always"` (default) or `"on-failure"`.
    #[config(default)]
    pub restart: RestartPolicy,
    /// Maximum number of restarts after the first start. `0` means no
    /// limit.
    #[config(default_t = 10)]
    pub max_restarts: u32,
    /// Apply process hardening (namespace isolation, no-new-privileges).
    /// True by default. It can be false also if the main shell is hardened
    /// (for example to run `dockerd`).
    #[config(default_t = true)]
    pub harden: bool,
    /// Environment variables for the daemon process. Values support
    /// `${VAR}` substitution from the host environment.
    #[config(default)]
    pub env: BTreeMap<String, String>,
}

impl WellKnown for Daemon {
    type Deserializer = de::Nested<Daemon>;
    const DE: Self::Deserializer = de::nested();
}

/// Hides subdirectories of the project mount from the sandbox in the VM.
/// An empty directory is put over each of them.
///
/// Each `paths` entry is relative to the project root and must be a plain
/// relative path. A leading `/` or `~` is an error. The masked tree is
/// made again at each VM start. Thus the config is the source of truth,
/// not the earlier state of the guest.
#[derive(Debug, Clone, serde::Serialize, DescribeConfig, DeserializeConfig)]
pub struct Mask {
    /// Enable or disable this mask.
    #[config(default_t = true)]
    pub enabled: bool,
    /// Paths to mask, relative to the project. They must not start with
    /// `/` or `~`, and must not contain `..`.
    pub paths: Vec<String>,
}

impl WellKnown for Mask {
    type Deserializer = de::Nested<Mask>;
    const DE: Self::Deserializer = de::nested();
}

/// Parse and validate a merged config document.
/// Args:
///  - `merged`: Merged config document. It must be a table.
///
/// Returns:
///   The config values, or an error that lists all problems.
pub(crate) fn parse(merged: serde_json::Value) -> anyhow::Result<ConfigValues> {
    let serde_json::Value::Object(map) = merged else {
        anyhow::bail!("config must be a TOML table");
    };

    let schema = smart_config::ConfigSchema::new(&ConfigValues::DESCRIPTION, "");
    let source = smart_config::Json::new("merged config", map);
    let repo = smart_config::ConfigRepository::new(&schema).with(source);
    let parser = repo.single::<ConfigValues>()?;
    let config = match parser.parse() {
        Ok(config) => config,
        Err(errors) => {
            return Err(anyhow::anyhow!(format_error(
                "invalid configuration",
                errors,
            )));
        }
    };

    validate(&config)?;

    Ok(config)
}

/// Check a parsed config for problems that the schema cannot express (see
/// [`validate_network`]).
pub(crate) fn validate(config: &ConfigValues) -> anyhow::Result<()> {
    validate_network(config)
}

/// Do the cross-field checks of `[network]` that the schema cannot
/// express. The errors have the same form as smart-config parse errors,
/// thus the user sees one "invalid configuration" block.
///
/// Checks:
///  - Target patterns: each `allow` and `deny` entry of an enabled rule,
///    and each `target` entry of an enabled middleware, must have a port
///    that is a number or `*` (or no port). Otherwise the proxy must
///    select a meaning for `*:8O80`. The only safe meaning is "do not
///    start". With "any port", a typo becomes a wide-open allow under
///    deny-by-default.
///  - Inject: each name in the `inject` list of an enabled rule must be an
///    `[env]` entry with `mask = true`. If the value is not masked, the
///    guest already has the real secret. An undefined name is a typo. A
///    rule with `inject` also cannot be `passthrough`, because injection
///    needs interception.
///  - Services: each name in `[network.services]` must be a known service.
fn validate_network(config: &ConfigValues) -> anyhow::Result<()> {
    let mut problems: Vec<String> = Vec::new();
    for name in config.network.services.keys() {
        if ServiceId::from_name(name).is_none() {
            let known: Vec<&str> = ServiceId::ALL.iter().map(|s| s.name()).collect();
            problems.push(format!(
                "* `network.services.{name}` unknown service (known: {})",
                known.join(", ")
            ));
        }
    }
    for (rule_name, rule) in &config.network.rules {
        if !rule.enabled {
            continue;
        }
        for (field, patterns) in [("allow", &rule.allow), ("deny", &rule.deny)] {
            for pattern in patterns {
                if let Err(e) = parse_pattern(pattern) {
                    problems.push(format!("* `network.rules.{rule_name}.{field}` {e}"));
                }
            }
        }
        if rule.passthrough && !rule.inject.is_empty() {
            problems.push(format!(
                "* `network.rules.{rule_name}` inject cannot be combined with passthrough"
            ));
        }
        for var in &rule.inject {
            let masked = config.env.get(var).is_some_and(|e| e.mask);
            if !masked {
                problems.push(format!(
                    "* `network.rules.{rule_name}.inject` `{var}` must be defined in [env] with mask = true"
                ));
            }
        }
    }
    for (mw_name, mw) in &config.network.middleware {
        if !mw.enabled {
            continue;
        }
        for pattern in &mw.target {
            if let Err(e) = parse_pattern(pattern) {
                problems.push(format!("* `network.middleware.{mw_name}.target` {e}"));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("invalid configuration\n{}", problems.join("\n"));
    }
}

#[cfg(test)]
mod tests {
    //! Tests for image references.

    use super::*;

    /// Test that an image reference gives a pinned digest only if the digest is
    /// a full sha256 value after a name.
    ///   1. Check references with a digest, a tag and a registry port
    ///   2. Check references without a digest, or with a short, bad or nameless
    ///      digest
    #[test]
    fn pinned_digest_is_found_only_in_well_formed_references() {
        let digest = format!("sha256:{}", "a".repeat(64));
        for name in [
            format!("alpine@{digest}"),
            format!("alpine:3.20@{digest}"),
            format!("localhost:5005/alpine:3@{digest}"),
        ] {
            assert_eq!(ImageRef::auto(name).pinned_digest(), Some(digest.as_str()));
        }
        for name in [
            "alpine".to_string(),
            "alpine:3.20".to_string(),
            "localhost:5005/alpine:3".to_string(),
            "alpine@sha256".to_string(),
            "alpine@sha256:abc".to_string(),
            format!("alpine@sha256:{}", "z".repeat(64)),
            format!("@{digest}"),
        ] {
            assert_eq!(ImageRef::auto(name.clone()).pinned_digest(), None, "{name}");
        }
    }
}
