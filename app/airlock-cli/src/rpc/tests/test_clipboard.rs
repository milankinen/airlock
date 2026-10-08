//! Tests for guest copy and paste through the host clipboard programs:
//! grants, size limits and failing programs.

use std::path::Path;

use airlock_common::supervisor_capnp::clipboard;

use crate::rpc::clipboard::ClipboardImpl;
use crate::test_cfg::{block_on_local, rpc_loopback, temp_dir};

/// Leak `args` to get the static argv that the clipboard needs.
fn argv(args: &[String]) -> &'static [&'static str] {
    let args: Vec<&'static str> = args
        .iter()
        .map(|a| &*Box::leak(a.clone().into_boxed_str()))
        .collect();
    Box::leak(args.into_boxed_slice())
}

/// A host clipboard that keeps its data in the file `clip`, served to the
/// guest over RPC.
/// Args:
///  - `copy`, `paste`: grant each direction
///  - `limit`: maximum copy size in bytes
fn file_clipboard(clip: &Path, copy: bool, paste: bool, limit: u64) -> clipboard::Client {
    let clip = clip.display().to_string();
    let write = argv(&["sh".into(), "-c".into(), format!("cat > '{clip}'")]);
    let read = argv(&["cat".into(), clip]);
    served(ClipboardImpl::with_programs(
        write, read, copy, paste, limit,
    ))
}

/// Serve `clipboard` to a guest client over RPC.
fn served(clipboard: ClipboardImpl) -> clipboard::Client {
    rpc_loopback(capnp_rpc::new_client::<clipboard::Client, _>(clipboard).client)
}

/// Copy `data` from the guest. Returns the RPC error message on failure.
async fn copy(client: &clipboard::Client, data: &[u8]) -> Result<(), String> {
    let mut request = client.copy_request();
    request.get().set_data(data);
    request
        .send()
        .promise
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Paste from the guest. Returns the data or the RPC error message.
async fn paste(client: &clipboard::Client) -> Result<Vec<u8>, String> {
    let response = client
        .paste_request()
        .send()
        .promise
        .await
        .map_err(|e| e.to_string())?;
    Ok(response.get().unwrap().get_data().unwrap().to_vec())
}

/// Test that guest copy and paste use the host clipboard programs, and
/// that a copy over the size limit does not change the host clipboard.
///   1. Copy from the guest and check the host clipboard file
///   2. Write the host clipboard file and paste it in the guest
///   3. Copy 9 bytes with an 8 byte limit and check the error
///   4. Check that the host clipboard did not change
#[test]
fn guest_copy_and_paste_go_through_host_clipboard_program() {
    let tmp = temp_dir();
    let clip = tmp.path().join("clip");
    block_on_local(async {
        let client = file_clipboard(&clip, true, true, 8);

        copy(&client, b"hello").await.unwrap();
        assert_eq!(std::fs::read(&clip).unwrap(), b"hello");
        std::fs::write(&clip, b"from host").unwrap();
        assert_eq!(paste(&client).await.unwrap(), b"from host");

        let err = copy(&client, b"123456789").await.unwrap_err();
        assert!(err.contains("exceeds the 8 byte limit"), "{err}");
        assert_eq!(std::fs::read(&clip).unwrap(), b"from host");
    });
}

/// Test that the guest cannot use a clipboard direction that the user did
/// not grant.
///   1. Copy with a paste-only clipboard and check the error
///   2. Check that the host clipboard did not change
///   3. Paste with a copy-only clipboard and check the error
#[test]
fn ungranted_clipboard_direction_is_refused() {
    let tmp = temp_dir();
    let clip = tmp.path().join("clip");
    std::fs::write(&clip, b"secret").unwrap();
    block_on_local(async {
        let paste_only = file_clipboard(&clip, false, true, 8);
        let err = copy(&paste_only, b"x").await.unwrap_err();
        assert!(err.contains("copy is not granted"), "{err}");
        assert_eq!(std::fs::read(&clip).unwrap(), b"secret");

        let copy_only = file_clipboard(&clip, true, false, 8);
        let err = paste(&copy_only).await.unwrap_err();
        assert!(err.contains("paste is not granted"), "{err}");
    });
}

/// Test that a host clipboard program that fails gives a copy error and an
/// empty paste.
///   1. Use a program that does not exist, then a program that exits with
///      an error
///   2. Check that the copy fails and the paste returns no data
#[test]
fn failing_host_clipboard_program_fails_copy_and_pastes_empty() {
    block_on_local(async {
        for program in ["airlock-definitely-not-a-real-program", "false"] {
            let client = served(ClipboardImpl::with_programs(
                argv(&[program.into()]),
                argv(&[program.into()]),
                true,
                true,
                8,
            ));
            assert!(copy(&client, b"hi").await.is_err(), "{program}");
            assert!(paste(&client).await.unwrap().is_empty(), "{program}");
        }
    });
}
