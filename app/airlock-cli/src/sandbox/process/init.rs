//! Init protocol between the sandbox process and its parent.
//!
//! The parent gives the sandbox, the held locks and the start options to
//! the sandbox process, and waits until the sandbox is ready:
//!  1. The parent sends the init data.
//!  2. The sandbox process does its init and serves the first client
//!     connection of the parent. Then it tells that the sandbox is ready.
//!  3. The parent attaches to the sandbox and tells the sandbox process to
//!     detach.
//!  4. The sandbox process frees the startup lock, leaves the terminal and
//!     tells that it detached. Then it closes the channel.
//!
//! A closed channel before the ready message means that the init failed.
//! A closed channel before the detach message means that the parent
//! stopped. A closed channel with no detached message means that the
//! sandbox process stopped. Each message has a size limit.

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::cli::LogLevel;
use crate::config::config_values::Policy;
use crate::messages;

/// Size limit of the init message line. The start arguments can be long.
pub const INIT_CAP: u64 = 1024 * 1024;

/// Size limit of the ready message line. The main command has the start
/// arguments and the command of the image.
pub const READY_CAP: u64 = 2 * INIT_CAP;

/// Size limit of the other message lines.
pub const MESSAGE_CAP: u64 = 64 * 1024;

/// Init data of the sandbox process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Init {
    /// Registry id of the sandbox.
    pub sandbox_id: String,
    /// Inherited descriptor of the held run lock.
    pub run_lock_fd: i32,
    /// Inherited descriptor of the held startup lock.
    pub startup_lock_fd: i32,
    /// Inherited descriptor of the RPC socket of the parent. It is the
    /// first client connection of the sandbox.
    pub rpc_fd: i32,
    /// Log and output options.
    pub log: LogOpts,
    /// What the sandbox process does in its init.
    pub mode: Mode,
}

/// Log and output options of the sandbox process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogOpts {
    /// Log level of the sandbox log and the guest (`--log-level`).
    pub log_level: LogLevel,
    /// Show the detailed boot output (`--verbose`).
    pub verbose: bool,
    /// Do not show status messages and progress (`--quiet`).
    pub quiet: bool,
}

/// What the sandbox process does in its init.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Prepare and start the sandbox for `airlock start`.
    Start {
        /// The start arguments.
        args: StartArgs,
    },
    /// Start the sandbox with the last config for `airlock exec`.
    Exec,
    /// Start the sandbox with the last config for `airlock acp`, with no
    /// questions.
    Acp,
}

/// Arguments of `airlock start` for the sandbox process.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[allow(clippy::struct_excessive_bools)] // independent flags
pub struct StartArgs {
    /// Command and arguments after `--`. Empty for the image command.
    pub args: Vec<String>,
    /// Run the main command in a login shell (`--login`).
    pub login: bool,
    /// Network policy for this run (`--network`).
    pub network: Option<Policy>,
    /// Working directory in the container (`--sandbox-cwd`). `None` is the
    /// project directory.
    pub sandbox_cwd: Option<String>,
    /// Use the default answer for every sandbox question (`--yes`).
    pub yes: bool,
    /// Accept config changes without a question
    /// (`--allow-configuration-changes`).
    pub allow_config_changes: bool,
    /// Approve new or changed packs without a question
    /// (`--auto-approve-packs`).
    pub auto_approve_packs: bool,
}

/// The sandbox is ready: clients can connect.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Ready {
    /// Main command of `airlock start`. `None` for the other modes.
    #[serde(default)]
    pub main_argv: Option<Vec<String>>,
}

/// A message from the parent to the sandbox process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToChild {
    /// The init data. Always the first message.
    Init(Init),
    /// The parent connected. The sandbox process can detach.
    Detach,
}

/// A message from the sandbox process to the parent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToParent {
    /// The sandbox is ready.
    Ready(Ready),
    /// The sandbox process detached. The end of the stream comes next.
    Detached,
}

/// Read one message line from `reader`.
/// Args:
///  - `reader`: The stream
///  - `cap`: Size limit of the line, [`INIT_CAP`], [`READY_CAP`] or
///    [`MESSAGE_CAP`]
///
/// Returns:
///   The message, or `None` at the end of the stream. Error for a read
///   error, a line over the size limit, or a line that is not a message.
pub async fn read_message<T: serde::de::DeserializeOwned>(
    reader: &mut (impl AsyncBufRead + Unpin),
    cap: u64,
) -> anyhow::Result<Option<T>> {
    let mut line = Vec::new();
    reader.take(cap + 1).read_until(b'\n', &mut line).await?;
    if line.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        line.len() as u64 <= cap,
        messages::sandbox_process::bad_init_message("too large")
    );
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|e| anyhow::anyhow!(messages::sandbox_process::bad_init_message(e)))
}

/// Write `message` as one line to `writer`.
/// Returns:
///   Error if the message does not encode or the write fails.
pub async fn write_message<T: serde::Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &T,
) -> anyhow::Result<()> {
    // JSON text has no raw newline. Thus the message is one line.
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::io::BufReader;

    use super::*;
    use crate::test_cfg::block_on_local;

    /// Test that the messages go through one line each, that the end of
    /// the stream gives `None`, and that a line over the size limit is
    /// refused. A broken parent must not make the sandbox process read
    /// without a limit.
    ///   1. Write an init and a detach message and close the writer
    ///   2. Read both messages and the end of the stream
    ///   3. Read a line over the size limit and check the error
    #[test]
    fn messages_round_trip_and_large_line_is_refused() {
        block_on_local(async {
            let init = Init {
                sandbox_id: "abc".into(),
                run_lock_fd: 5,
                startup_lock_fd: 6,
                rpc_fd: 7,
                log: LogOpts {
                    log_level: LogLevel::Debug,
                    verbose: true,
                    quiet: false,
                },
                mode: Mode::Start {
                    args: StartArgs {
                        args: vec!["echo".into(), "a\nb".into()],
                        login: true,
                        network: Some(Policy::DenyAlways),
                        sandbox_cwd: Some("/work".into()),
                        yes: true,
                        ..StartArgs::default()
                    },
                },
            };
            let mut bytes = Vec::new();
            write_message(&mut bytes, &ToChild::Init(init.clone()))
                .await
                .unwrap();
            write_message(&mut bytes, &ToChild::Detach).await.unwrap();
            assert_eq!(String::from_utf8_lossy(&bytes).lines().count(), 2);

            let mut reader = BufReader::new(bytes.as_slice());
            let first: Option<ToChild> = read_message(&mut reader, INIT_CAP).await.unwrap();
            assert_eq!(first, Some(ToChild::Init(init)));
            let second: Option<ToChild> = read_message(&mut reader, INIT_CAP).await.unwrap();
            assert_eq!(second, Some(ToChild::Detach));
            let end: Option<ToChild> = read_message(&mut reader, INIT_CAP).await.unwrap();
            assert_eq!(end, None);

            let large = vec![b'x'; usize::try_from(MESSAGE_CAP).unwrap() + 10];
            let mut reader = BufReader::new(large.as_slice());
            let err = read_message::<ToChild>(&mut reader, MESSAGE_CAP)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("too large"), "{err}");
        });
    }
}
