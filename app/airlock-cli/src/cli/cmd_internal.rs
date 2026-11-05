//! The hidden `airlock internal` commands.
//!
//! Only airlock starts these commands. `vmm` runs the sandbox process of
//! one sandbox. Its parent gives it the init socket and the held sandbox
//! locks. `vm-libkrun` runs the libkrun VM of one sandbox. It
//! starts before all other parts of the program.

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
    /// Run the libkrun VM process of one sandbox (VM description on stdin)
    #[cfg(vm_libkrun)]
    #[command(name = VMM_LIBKRUN_ARGS[1])]
    VmmLibkrun,
}

/// Arguments that start the sandbox process.
pub const SANDBOX_PROCESS_ARGS: [&str; 2] = ["internal", "vmm"];

/// Arguments that start the libkrun VM process.
#[cfg(vm_libkrun)]
pub const VMM_LIBKRUN_ARGS: [&str; 2] = ["internal", "vm-libkrun"];

/// Check if the arguments of this process start the libkrun VM process.
/// The VM process must start before the context, the logs and the
/// signal handlers, so `main` checks this before the argument parse.
#[cfg(vm_libkrun)]
pub fn is_vmm_libkrun() -> bool {
    std::env::args_os().skip(1).eq(VMM_LIBKRUN_ARGS)
}

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
        // `main` starts the VM process before the context. This is the same
        // entry, for completeness.
        #[cfg(vm_libkrun)]
        InternalCommand::VmmLibkrun => crate::vm::libkrun::vmm_main(),
    }
}
