//! Sandbox lifecycle.
//!
//! Runs a sandbox VM for a locked project:
//!  * boots a VM with all the access that the configuration gives the guest
//!  * starts processes in the VM and relays their input and output
//!  * shows the progress of the preparation and boot
//!  * runs the interactive sandbox of `airlock start`
//!  * stops the VM in the correct order
//!
//! If a step fails, it removes what it built. Thus a caller must stop only
//! what it holds.

pub mod boot;
pub mod interactive;
pub mod io;
pub mod report;
mod tasks;
pub mod vm;

use crate::oci::{self, OciImage};

/// Error: a boot step got the user's Ctrl+C or SIGTERM and stopped. The step
/// already removed what it built. Commands map this error to exit code 130.
#[derive(Debug, thiserror::Error)]
#[error("interrupted")]
pub struct Interrupted;

/// Get the argv of the main process.
/// Args:
///  - `args`: The command after `--`. If empty, the image command is used.
///  - `login`: If `true`, run the command in a login shell
///  - `image`: Image with the default command
///
/// Returns:
///   The argv of the main process.
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
    //! Tests for the command of the main guest process.

    use super::*;

    /// The `args` as owned strings.
    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    /// An image whose default command is `cmd`.
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

    /// Test that the main command is the user command or else the image
    /// command, and that a login shell wraps it when asked.
    ///   1. Check that no args give the image command
    ///   2. Check that args replace the image command
    ///   3. Check that a login shell with no args runs the image shell with
    ///      `-l`
    ///   4. Check that a login shell with args runs them through `exec` in
    ///      `bash -l -c`, with each arg kept as one word
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
