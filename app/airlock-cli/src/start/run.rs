//! The last step of `airlock start`.
//!
//! Runs the sandbox session with the run config of the project.

use std::rc::Rc;

use super::Exit;
use crate::approval::Approver;
use crate::cli::cmd_start::StartArgs;
use crate::oci::OciImage;
use crate::project::Project;
use crate::runtime::HostRuntime;
use crate::sandbox;

/// Run the sandbox session.
/// Args:
///  - `project`: The open project
///  - `image`: Container image of the sandbox
///  - `extra_args`: Arguments after `--`, for the container command
///  - `args`: Command-line arguments of `airlock start`
///  - `runtime`: Terminal runtime
///  - `approver`: Asks the user to approve requests from the sandbox
///
/// Returns:
///   Exit code of the sandbox, or error.
// The session sets the network rules, boots the VM, starts the supervisor
// RPC, relays I/O and then shuts down in order.
pub async fn run_sandbox(
    project: Project,
    image: &OciImage,
    extra_args: Vec<String>,
    args: &StartArgs,
    runtime: HostRuntime,
    approver: Rc<dyn Approver>,
) -> Result<i32, Exit> {
    let argv = sandbox::main_argv(extra_args, args.login, image);
    let code = Box::pin(sandbox::interactive::run_interactive(
        project,
        image,
        argv,
        runtime,
        approver,
        args.log_level,
    ))
    .await?;
    Ok(code)
}
