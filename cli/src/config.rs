use std::path::PathBuf;

pub struct Config {
    pub image: String,
    pub cpus: u32,
    pub memory_mb: u64,
    pub verbose: bool,
    pub bundle_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            image: "alpine:latest".to_owned(),
            cpus: 2,
            memory_mb: 512,
            verbose: false,
            bundle_path: ".tmp/bundle".into(),
        }
    }
}
