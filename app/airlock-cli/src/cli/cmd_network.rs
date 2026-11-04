//! The `airlock network` command.
//!
//! Adds port forwards between the host and a running sandbox while the
//! command runs:
//!  * to the sandbox: host programs connect to a host loopback port and
//!    reach a port on the sandbox loopback
//!  * to the host: sandbox programs connect to a sandbox loopback port and
//!    reach a port on the host loopback
//!
//! The sandbox process relays the connections. The forwards do not hold
//! the sandbox, and the command does not start it. The forwards close when the
//! command stops or the sandbox stops. A port conflict stops all forwards
//! of the command.

use std::collections::HashSet;
use std::rc::Rc;

use clap::{Args, Subcommand, ValueEnum};

use crate::config::config_values::PortMapping;
use crate::context::Context;
use crate::sandbox::{FindOpts, ForwardMode, ForwardReport};
use crate::{cli, messages, sandbox};

/// CLI arguments for `airlock network`.
#[derive(Args, Debug)]
pub struct NetworkArgs {
    #[command(subcommand)]
    pub command: NetworkCommand,
}

/// Subcommands of `airlock network`.
#[derive(Subcommand, Debug)]
pub enum NetworkCommand {
    /// Forward ports between the host and the running sandbox until Ctrl+C
    Forward(ForwardArgs),
}

/// Direction of the forwards.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Host programs connect to sandbox ports (same as config `guest` list)
    Guest,
    /// Sandbox programs connect to host ports (same as config `host` list)
    Host,
}

impl From<Mode> for ForwardMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Guest => Self::Guest,
            Mode::Host => Self::Host,
        }
    }
}

/// CLI arguments for `airlock network forward`.
#[derive(Args, Debug)]
pub struct ForwardArgs {
    /// Direction of the forwards
    #[arg(long, value_enum, default_value_t = Mode::Guest)]
    pub mode: Mode,
    /// In a git worktree, also look for the sandbox of the main repository
    #[arg(long)]
    pub follow_git_worktree: bool,
    /// Forwards: PORT (same port on both sides) or HOST_PORT:SANDBOX_PORT
    #[arg(required = true, value_name = "FORWARD")]
    pub forwards: Vec<PortMapping>,
}

/// Entry point for `airlock network`.
/// Returns:
///   Exit code 0 after Ctrl+C. Error if the forwards cannot start or the
///   sandbox stops.
pub async fn main(args: NetworkArgs, context: &Context) -> anyhow::Result<i32> {
    let NetworkCommand::Forward(args) = args.command;
    // Check the arguments before the connection to the sandbox.
    check_duplicates(args.mode, &args.forwards)?;
    let opts = FindOpts {
        parents: true,
        follow_git_worktree: args.follow_git_worktree,
    };
    let process = match sandbox::find(context, &context.work, opts).await? {
        Some(sandbox) => sandbox.connect(context).await?,
        None => anyhow::bail!(messages::cli::no_running_sandbox(&context.work)),
    };
    let mode = args.mode.into();
    process
        .forward(mode, &args.forwards, cli::interrupted(), Rc::new(Console))
        .await?;
    Ok(0)
}

/// Make sure that no two forwards use the same listening port: the host
/// port in guest mode, and the sandbox port in host mode.
fn check_duplicates(mode: Mode, forwards: &[PortMapping]) -> anyhow::Result<()> {
    let mut seen = HashSet::new();
    for m in forwards {
        let (port, message): (u16, fn(u16) -> String) = match mode {
            Mode::Guest => (m.host, messages::network_forward::duplicate_host_port),
            Mode::Host => (m.guest, messages::network_forward::duplicate_guest_port),
        };
        anyhow::ensure!(seen.insert(port), message(port));
    }
    Ok(())
}

/// Report on the console.
struct Console;

impl ForwardReport for Console {
    fn ready(&self, mode: ForwardMode, forwards: &[PortMapping]) {
        let header = match mode {
            ForwardMode::Guest => messages::network_forward::FORWARDING_TO_SANDBOX,
            ForwardMode::Host => messages::network_forward::FORWARDING_TO_HOST,
        };
        cli::log!("{} {header}", cli::check());
        for m in forwards {
            let line = match mode {
                ForwardMode::Guest => messages::network_forward::to_sandbox_line(m.host, m.guest),
                ForwardMode::Host => messages::network_forward::to_host_line(m.guest, m.host),
            };
            cli::log!("  {} {line}", cli::bullet());
        }
    }

    fn connect_failed(&self, guest_port: u16, reason: &str) {
        cli::log!(
            "{} {}",
            cli::yellow("!"),
            messages::network_forward::connect_failed(guest_port, reason)
        );
    }
}
