//! Sandbox API: boot a VM for a locked project with every capability the
//! guest gets, start processes in it, and shut everything down in order.
//!
//! ```text
//! oci::prepare ─► boot::boot(BootSpec) ─► Vm ─► Vm::spawn(..) … ─► Vm::shutdown
//! ```
//!
//! The caller builds everything the guest gets before the boot (network,
//! browser, clipboard, daemons, masks, env; see [`boot::BootSpec`]); the
//! boot only wires it and starts no process. Processes — the main process
//! of `airlock start`, the installers of the install boot, `airlock exec`
//! — start afterwards with [`vm::Vm::spawn`].
//!
//! Each step tears down what it built when it fails, so a caller only has
//! to shut down the value it holds. Every background task of a boot is
//! owned by that boot (see [`tasks::BootTasks`]) and stops before the next
//! boot in the same process can start. `airlock start` is
//! [`interactive::run_interactive`] over this API.

pub mod boot;
pub mod interactive;
pub mod io;
pub mod report;
mod tasks;
pub mod vm;

use crate::oci::{self, OciImage};

/// A boot step saw the user's Ctrl+C / SIGTERM and stopped. What it had
/// built is already torn down; commands map this to exit code 130.
#[derive(Debug, thiserror::Error)]
#[error("interrupted")]
pub struct Interrupted;

/// The main process argv: `args` (the command after `--`) or the image's
/// command when empty, wrapped in a login shell when `login` is set.
pub fn main_argv(args: Vec<String>, login: bool, image: &OciImage) -> Vec<String> {
    let cmd = if args.is_empty() {
        image.cmd.clone()
    } else {
        args
    };
    if login {
        oci::apply_login_shell(cmd)
    } else {
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    fn image(cmd: &[&str]) -> OciImage {
        OciImage {
            image_id: "sha256:test".into(),
            name: "test".into(),
            image_layers: vec![],
            container_home: "/root".into(),
            uid: 0,
            gid: 0,
            cmd: argv(cmd),
            env: vec![],
            user: None,
        }
    }

    #[test]
    fn main_command_is_args_or_image_command_optionally_in_login_shell() {
        assert_eq!(main_argv(vec![], false, &image(&["/bin/sh"])), ["/bin/sh"]);
        assert_eq!(
            main_argv(argv(&["cat", "-n"]), false, &image(&["/bin/sh"])),
            ["cat", "-n"]
        );
        assert_eq!(
            main_argv(vec![], true, &image(&["/bin/bash"])),
            ["/bin/bash", "-l"]
        );
        assert_eq!(
            main_argv(argv(&["claude", "--yolo"]), true, &image(&["/bin/bash"])),
            ["bash", "-l", "-c", r#"exec "$0" "$@""#, "claude", "--yolo"]
        );
    }
}
