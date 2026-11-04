//! Tests for a sandbox that does not start for an agent.

use std::ffi::OsString;

use serde_json::json;

use crate::acp::agent::launcher::SandboxProcessLauncher;
use crate::cli::LogLevel;
use crate::sandbox::process::client::{LaunchOpts, ProcessLauncher};
use crate::sandbox::process::init::Mode;
use crate::test_cfg::acp::{TestClient, test_config};
use crate::test_cfg::oci::verified_dir;
use crate::test_cfg::sandboxes::data_dir_sandbox;
use crate::test_cfg::{block_on_local, temp_dir, test_context};
use crate::vault::{Vault, VaultStorageType};

/// A sandbox process that reads the init data, writes progress and an
/// error, and fails, as for a locked vault.
const FAILING_SANDBOX_PROCESS: &str = r"read -r init <&$1
echo 'loading the last config'
echo 'error: the vault is locked' >&2
exit 4";

/// Test that the session request of an agent whose sandbox process fails
/// gets the error output of the sandbox process. The editor shows only
/// the reply, so the reason must be in it.
///   1. Register a sandbox with the config of a last start
///   2. Use a sandbox process that writes an error and exits with code 4
///   3. Send `session/new` for the project
///   4. Check that the error message is the last error line, and that the
///      error data has the exit code and the output
#[test]
fn sandbox_process_failure_reply_has_captured_output() {
    let home = temp_dir();
    let context = test_context(
        home.path(),
        Vault::for_storage_type(VaultStorageType::Disabled),
    );
    let project_tmp = temp_dir();
    let project = std::fs::canonicalize(project_tmp.path()).unwrap();
    let (_, dir) = data_dir_sandbox(&context, &project);
    // A sandbox that `airlock start` prepared: a disk and a last config.
    std::fs::write(dir.join(crate::vm::disk::DISK_FILE), b"").unwrap();
    crate::sandbox::meta::save_last_run_config(
        &verified_dir(&dir),
        json!({}),
        crate::sandbox::meta::HostPaths::default(),
    )
    .unwrap();
    let opts = LaunchOpts {
        log_level: LogLevel::Info,
        verbose: false,
        quiet: true,
        mode: Mode::Acp,
    };
    let args: Vec<OsString> = vec!["-c".into(), FAILING_SANDBOX_PROCESS.into(), "bash".into()];
    let process_launcher =
        ProcessLauncher::with_program(opts, verified_dir("/bin/bash".as_ref()), args);
    let launcher = SandboxProcessLauncher {
        context,
        follow_git_worktree: false,
        env: vec![],
        log_level: LogLevel::Info,
        launcher: Some(process_launcher),
    };

    block_on_local(async {
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": project.to_str().unwrap(), "mcpServers": [] });
        let (response, _) = client.request(2, "session/new", params).await;
        let error = &response["error"];
        assert_eq!(error["code"], -32603, "{response}");
        assert_eq!(error["message"], "error: the vault is locked");
        assert_eq!(error["data"]["exitCode"], 4);
        let output = error["data"]["output"].as_str().unwrap();
        assert!(output.contains("loading the last config"), "{output}");
        assert!(output.contains("error: the vault is locked"), "{output}");
    });
}
