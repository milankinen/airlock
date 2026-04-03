pub struct Config {
    pub image: String,
    pub cpus: u32,
    pub memory_mb: u64,
    pub network: Network,
    pub verbose: bool,
    pub mounts: Vec<Mount>,
    pub args: Vec<String>,
    pub terminal: bool,
}

pub struct Mount {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

pub struct Network {
    /// Exposed local host ports
    pub host_ports: Vec<u16>
}

impl Default for Config {
    fn default() -> Self {
        Self {
            image: "ezpez-tester:latest".to_owned(),
            cpus: 2,
            memory_mb: 512,
            network: Network {
                host_ports: vec![9999]
            },
            verbose: false,
            mounts: vec![],
            args: vec![],
            terminal: true,
        }
    }
}
