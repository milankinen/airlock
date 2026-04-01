use clap::Parser;

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

    /// Enable verbose output (show kernel boot messages)
    #[arg(short, long)]
    pub verbose: bool,
}
