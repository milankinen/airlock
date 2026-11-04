//! Access of a sandbox session.
//!
//! Builds all the access that the project config gives the guest of a
//! session: network services, browser access, the sandbox network, the
//! env, daemons, directory masks, clipboard and MCP servers. The project
//! directory is shared. Also stops a booted session after an error.

use std::rc::Rc;

use super::boot::{BootOptions, BootSpec, guest_env};
use super::machine::Vm;
use crate::approval::Approver;
use crate::network::{self, Network};
use crate::oci::{self, OciImage};
use crate::rpc::browser::Browser;
use crate::rpc::clipboard;
use crate::sandbox::Project;
use crate::services::{self, Services};
use crate::{daemon, masking, mcp};

/// What a session boot needs, and the parts that the caller keeps.
pub struct SessionBoot<'a> {
    /// The spec for [`super::boot::boot`].
    pub spec: BootSpec<'a>,
    /// The network services. Attach them after the boot, and detach them
    /// before the shutdown.
    pub services: Services,
    /// The browser bridge, for its notices. `None` gives no browser access.
    pub browser: Option<Browser>,
}

/// Build the boot of a session with all access that the config of
/// `project` gives.
/// Args:
///  - `project`: The locked project
///  - `image`: The container image
///  - `options`: Output and log options. The project share is always on.
///  - `approver`: Asks the user to approve requests from the sandbox
///
/// Returns:
///   The boot spec and the parts that the caller keeps, or error if a part
///   of the config is not valid.
pub fn session_boot(
    project: Project,
    image: &OciImage,
    mut options: BootOptions,
    approver: Rc<dyn Approver>,
) -> anyhow::Result<SessionBoot<'_>> {
    // The network config below expects the shared project dir.
    options.project_share = true;
    let tls_client = network::native_tls_client();
    let services = services::build_enabled(
        &project.config.network.services,
        &project.context,
        &tls_client,
    );
    let browser = Browser::new(services.browser_grants());
    let container_home = oci::effective_container_home(&project, image);
    let network = Network::new(
        &project,
        &container_home,
        tls_client,
        services.interceptors(),
        services.denied_targets(),
        true,
    )?
    .with_grants(network::GrantContext::for_project(&project, approver));
    let env = guest_env(&project, image, browser.is_some());
    let daemons = daemon::build_specs(&project, &env)?;
    let masks = masking::build_specs(&project)?;
    let clipboard = clipboard::for_config(&project.config.clipboard);
    let mcp = mcp::for_project(&project)?;
    let spec = BootSpec {
        project,
        image,
        options,
        env,
        network,
        browser: browser.clone(),
        clipboard,
        mcp,
        daemons,
        masks,
    };
    Ok(SessionBoot {
        spec,
        services,
        browser,
    })
}

/// Detach `services` and stop `vm` after `error`.
/// Returns:
///   The error to report: `error`, or the shutdown error (with `error` as
///   context) if the shutdown fails, because the VM may still run.
pub async fn shut_down_after(vm: Vm, services: &Services, error: anyhow::Error) -> anyhow::Error {
    services.detach().await;
    match Box::pin(vm.shutdown()).await {
        Ok(_) => error,
        Err(stop) => stop.context(format!("{error:#}")),
    }
}
