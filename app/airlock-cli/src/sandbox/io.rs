//! Guest process IO.
//!
//! Relays the output of a guest process to the host. Also gives a closed
//! input for processes that must not read input.

use std::rc::Rc;

use airlock_common::supervisor_capnp::stdin;

use crate::rpc;
use crate::runtime::OutputSink;

/// Relay the stdout and stderr of `proc` to `sink` until the process exits.
/// Returns:
///   The exit code of the process, or 1 if an RPC error occurs.
pub async fn drive(proc: &rpc::Process, sink: &mut impl OutputSink) -> i32 {
    // Traces contain only byte counts, never the content, because the output
    // of a sign-in tool can contain a token.
    loop {
        match proc.poll().await {
            Ok(rpc::ProcessEvent::Exit(code)) => return code,
            Ok(rpc::ProcessEvent::Stdout(data)) => {
                tracing::trace!("process stdout: {} bytes", data.len());
                sink.stdout(&data);
            }
            Ok(rpc::ProcessEvent::Stderr(data)) => {
                tracing::trace!("process stderr: {} bytes", data.len());
                sink.stderr(&data);
            }
            Err(e) => {
                tracing::error!("process poll error: {e}");
                return 1;
            }
        }
    }
}

/// Make a stdin that is at end of file from the start. The guest closes the
/// process stdin on the first read. For processes that must not read the
/// user's terminal (for example an installer in pipe mode).
pub fn closed_stdin() -> stdin::Client {
    capnp_rpc::new_client(ClosedStdin)
}

/// Stdin server that always replies with end of file.
struct ClosedStdin;

impl stdin::Server for ClosedStdin {
    async fn read(
        self: Rc<Self>,
        _params: stdin::ReadParams,
        mut results: stdin::ReadResults,
    ) -> Result<(), capnp::Error> {
        results.get().init_input().init_stdin().set_eof(());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the relay of guest process output and the closed stdin.

    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use airlock_common::supervisor_capnp::{data_frame, process, process_input};

    use super::*;
    use crate::test_cfg::sinks::RecordingSink;
    use crate::test_cfg::{block_on_local, rpc_loopback};

    /// One answer of [`ScriptedProcess`] to a `poll` call.
    enum Step {
        Stdout(&'static [u8]),
        Stderr(&'static [u8]),
        StdoutEof,
        StderrEof,
        Exit(i32),
    }

    /// A guest process that answers `poll` from a script. When the script
    /// ends, each call fails, as with a lost connection.
    struct ScriptedProcess(RefCell<VecDeque<Step>>);

    impl process::Server for ScriptedProcess {
        async fn poll(
            self: Rc<Self>,
            _params: process::PollParams,
            mut results: process::PollResults,
        ) -> Result<(), capnp::Error> {
            let step = self.0.borrow_mut().pop_front();
            let next = results.get().init_next();
            match step {
                Some(Step::Stdout(d)) => next.init_stdout().set_data(d),
                Some(Step::Stderr(d)) => next.init_stderr().set_data(d),
                Some(Step::StdoutEof) => next.init_stdout().set_eof(()),
                Some(Step::StderrEof) => next.init_stderr().set_eof(()),
                Some(Step::Exit(code)) => {
                    let mut next = next;
                    next.set_exit(code);
                }
                None => return Err(capnp::Error::failed("connection lost".into())),
            }
            Ok(())
        }
    }

    /// Relay the output of a scripted process to a recording sink.
    /// Returns:
    ///   The exit code and the recorded output.
    fn drive_script(script: Vec<Step>) -> (i32, RecordingSink) {
        block_on_local(async {
            let client: process::Client = rpc_loopback(
                capnp_rpc::new_client::<process::Client, _>(ScriptedProcess(RefCell::new(
                    script.into(),
                )))
                .client,
            );
            let mut sink = RecordingSink::default();
            let code = drive(&rpc::Process::new(client), &mut sink).await;
            (code, sink)
        })
    }

    /// Test that the relay sends stdout and stderr to the sink until the exit,
    /// and returns 1 when the connection is lost before the exit. An EOF
    /// marker does not stop the relay of its stream. The exit ends the
    /// relay, so no output gets lost.
    ///   1. Run a script with output on both streams, EOF markers and exit
    ///      code 3
    ///   2. Check the exit code and that each stream has all its output, also
    ///      the output after an EOF marker
    ///   3. Run a script that ends without an exit
    ///   4. Check that the exit code is 1 and the partial output arrived
    #[test]
    fn driving_process_relays_output_until_exit_or_lost_connection() {
        let (code, sink) = drive_script(vec![
            Step::Stdout(b"hello "),
            Step::Stderr(b"warn"),
            Step::StdoutEof,
            Step::Stdout(b"world"),
            Step::StderrEof,
            Step::Exit(3),
        ]);
        assert_eq!(
            (code, sink.out.as_slice(), sink.err.as_slice()),
            (3, &b"hello world"[..], &b"warn"[..])
        );

        let (code, sink) = drive_script(vec![Step::Stdout(b"partial")]);
        assert_eq!((code, sink.out.as_slice()), (1, &b"partial"[..]));
    }

    /// Test that the closed stdin answers the first read with end of file.
    ///   1. Read from the closed stdin
    ///   2. Check that the answer is a stdin EOF frame
    #[test]
    fn closed_stdin_reads_eof_at_once() {
        block_on_local(async {
            let response = closed_stdin().read_request().send().promise.await.unwrap();
            let input = response.get().unwrap().get_input().unwrap();
            let Ok(process_input::Stdin(frame)) = input.which() else {
                panic!("expected a stdin frame");
            };
            assert!(matches!(frame.unwrap().which(), Ok(data_frame::Eof(()))));
        });
    }
}
