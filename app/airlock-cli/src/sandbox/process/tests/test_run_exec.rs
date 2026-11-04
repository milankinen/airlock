//! Tests for the sandbox process of `airlock exec` when the sandbox does
//! not run. A sandbox with no config of a last start does not start. A
//! sandbox process that fails gives its exit code. A bash script takes the
//! place of the sandbox process, so no VM is necessary.

use std::ffi::OsString;

use serde_json::json;

use crate::cli::LogLevel;
use crate::sandbox::process::client::{LaunchOpts, ProcessLauncher};
use crate::sandbox::process::init::Mode;
use crate::sandbox::{ExitedBeforeReady, Run};
use crate::test_cfg::block_on_local;
use crate::test_cfg::cli_server::{TIMEOUT, registered_located};
use crate::test_cfg::oci::verified_dir;

/// A sandbox process that reads the init data, writes its working
/// directory to `start-cwd` and stops as after a Ctrl+C in a question.
const ABORTED_SANDBOX_PROCESS: &str = r"read -r init <&$1
pwd -P > start-cwd
exit 130";

/// Test that `exec` starts only a sandbox that `airlock start` prepared,
/// and gives the exit code of a sandbox process that stops in its init.
/// The sandbox process gave the reason on the terminal, and scripts see
/// the same code as from `airlock start`. Also that the sandbox process
/// runs in the project directory: the pack configs must see the same
/// directory as in the project.
///   1. Use a sandbox process that exits with code 130
///   2. Run `exec` on a stopped sandbox with no disk and no config of a
///      last start, and check the error that tells to run `airlock start`
///   3. Add a disk, and check the same error
///   4. Add the config of a last start, run `exec` again and check exit
///      code 130
///   5. Check that the sandbox process ran in the project directory
#[test]
fn exec_starts_only_prepared_sandbox_and_gives_exit_code_of_failed_start() {
    let (_home, context, located) = registered_located();
    block_on_local(async {
        let opts = LaunchOpts {
            log_level: LogLevel::Info,
            verbose: false,
            quiet: true,
            mode: Mode::Exec,
        };
        let args: Vec<OsString> = vec!["-c".into(), ABORTED_SANDBOX_PROCESS.into(), "bash".into()];
        let launcher =
            ProcessLauncher::with_program(opts, verified_dir("/bin/bash".as_ref()), args);
        let run = async || {
            let sandbox = located.sandbox.clone();
            sandbox.run_with(&context, Run::Exec, &launcher, TIMEOUT).await
        };

        let error = run().await.err().unwrap().to_string();
        assert!(error.contains("run 'airlock start' first"), "{error}");
        let dir = located.sandbox.dir();
        dir.join(crate::vm::disk::DISK_FILE)
            .unwrap()
            .write(b"")
            .unwrap();
        let error = run().await.err().unwrap().to_string();
        assert!(error.contains("run 'airlock start' first"), "{error}");

        let paths = crate::sandbox::meta::HostPaths::default();
        crate::sandbox::meta::save_last_run_config(dir, json!({}), paths).unwrap();
        let error = run().await.err().unwrap();
        assert_eq!(ExitedBeforeReady::code_of(&error), Some(130));

        let project = located.sandbox.project().unwrap().to_path().unwrap();
        let cwd = std::fs::read_to_string(project.join("start-cwd")).unwrap();
        assert_eq!(cwd.trim_end(), project.to_str().unwrap());
    });
}
