use clap::Parser;

/// Lightweight VM sandbox for running untrusted code
#[derive(Parser, Debug)]
#[command(name = "ez", version, about)]
pub struct Cli {
    /// Enable verbose output (show kernel boot messages)
    #[arg(short, long)]
    pub verbose: bool,

    /// Arguments passed to the entrypoint (e.g. ez -- -c 'echo hi')
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}
