//! The hidden `airlock internal` commands.
//!
//! Only airlock starts these commands. `vmm` runs the sandbox process of
//! one sandbox. Its parent gives it the init socket and the held sandbox
//! locks.

use clap::{Args, Subcommand};

use crate::context::Context;
use crate::sandbox;

/// CLI arguments for `airlock internal`.
#[derive(Args, Debug)]
pub struct InternalArgs {
    #[command(subcommand)]
    pub command: InternalCommand,
}

/// The internal commands.
#[derive(Subcommand, Debug)]
pub enum InternalCommand {
    /// Run the sandbox process of one sandbox
    #[command(name = SANDBOX_PROCESS_ARGS[1])]
    SandboxProcess(SandboxProcessArgs),
}

/// Arguments that start the sandbox process.
pub const SANDBOX_PROCESS_ARGS: [&str; 2] = ["internal", "vmm"];

/// CLI arguments for `airlock internal vmm`.
#[derive(Args, Debug)]
pub struct SandboxProcessArgs {
    /// Descriptor of the init socket
    pub init_fd: i32,
}

/// Entry point for `airlock internal`.
/// Returns:
///   Exit code of the command.
pub async fn main(args: InternalArgs, context: Context) -> i32 {
    match args.command {
        InternalCommand::SandboxProcess(args) => {
            Box::pin(sandbox::run_sandbox_process(args.init_fd, context)).await
        }
    }
}
