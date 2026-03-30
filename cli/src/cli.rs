use clap::Parser;
use std::path::PathBuf;

/// Lightweight VM sandbox for running untrusted code
#[derive(Parser, Debug)]
#[command(name = "ez", version, about)]
pub struct Cli {
    /// Number of virtual CPUs
    #[arg(long, default_value = "2")]
    pub cpus: u32,

    /// Memory size in megabytes
    #[arg(long, default_value = "512")]
    pub memory: u64,

    /// Path to a custom kernel (overrides default)
    #[arg(long)]
    pub kernel: Option<PathBuf>,

    /// Path to a custom initramfs (overrides default)
    #[arg(long)]
    pub initramfs: Option<PathBuf>,

    /// Enable verbose output (show kernel boot messages)
    #[arg(short, long)]
    pub verbose: bool,
}
