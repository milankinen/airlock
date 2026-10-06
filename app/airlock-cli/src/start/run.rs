//! The last step of `airlock start`: run the sandbox session with the
//! project's run config.

use super::Exit;
use crate::cli::cmd_start::StartArgs;
use crate::oci::OciImage;
use crate::project::Project;
use crate::runtime::HostRuntime;
use crate::sandbox;

/// Run the sandbox session. Returns the sandbox's exit code.
pub async fn run_sandbox(
    project: Project,
    image: &OciImage,
    extra_args: Vec<String>,
    args: &StartArgs,
    runtime: HostRuntime,
) -> Result<i32, Exit> {
    let argv = sandbox::main_argv(extra_args, args.login, image);
    let code = Box::pin(sandbox::interactive::run_interactive(
        project,
        image,
        argv,
        runtime,
        args.log_level,
    ))
    .await?;
    Ok(code)
}
