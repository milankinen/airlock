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

    fn image(cmd: &[&str]) -> OciImage {
        OciImage {
            image_id: "sha256:test".into(),
            name: "test".into(),
            image_layers: vec![],
            container_home: "/root".into(),
            uid: 0,
            gid: 0,
            cmd: cmd.iter().map(ToString::to_string).collect(),
            env: vec![],
            user: None,
        }
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn main_argv_prefers_args_over_the_image_command() {
        let img = image(&["/bin/sh"]);
        assert_eq!(main_argv(vec![], false, &img), argv(&["/bin/sh"]));
        assert_eq!(
            main_argv(argv(&["cat", "-n"]), false, &img),
            argv(&["cat", "-n"])
        );
    }

    #[test]
    fn main_argv_wraps_in_a_login_shell() {
        let img = image(&["/bin/bash"]);
        assert_eq!(main_argv(vec![], true, &img), argv(&["/bin/bash", "-l"]));
        assert_eq!(
            main_argv(argv(&["claude", "--yolo"]), true, &img),
            argv(&["bash", "-l", "-c", r#"exec "$0" "$@""#, "claude", "--yolo"])
        );
    }
}
