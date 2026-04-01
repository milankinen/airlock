pub struct Config {
    pub image: String,
    pub cpus: u32,
    pub memory_mb: u64,
    pub verbose: bool,
    pub mounts: Vec<Mount>
}

pub struct Mount {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            image: "alpine:latest".to_owned(),
            cpus: 2,
            memory_mb: 512,
            verbose: false,
            mounts: vec![]
        }
    }
}
