use std::path::Path;

use airlock_common::supervisor_capnp::clipboard;

use crate::rpc::clipboard::ClipboardImpl;
use crate::test_cfg::{block_on_local, rpc_loopback, temp_dir};

fn argv(args: &[String]) -> &'static [&'static str] {
    let args: Vec<&'static str> = args
        .iter()
        .map(|a| &*Box::leak(a.clone().into_boxed_str()))
        .collect();
    Box::leak(args.into_boxed_slice())
}

/// A host clipboard kept in the file `clip`, served to the guest.
fn file_clipboard(clip: &Path, copy: bool, paste: bool, limit: u64) -> clipboard::Client {
    let clip = clip.display().to_string();
    let write = argv(&["sh".into(), "-c".into(), format!("cat > '{clip}'")]);
    let read = argv(&["cat".into(), clip]);
    served(ClipboardImpl::with_programs(
        write, read, copy, paste, limit,
    ))
}

fn served(clipboard: ClipboardImpl) -> clipboard::Client {
    rpc_loopback(capnp_rpc::new_client::<clipboard::Client, _>(clipboard).client)
}

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

async fn paste(client: &clipboard::Client) -> Result<Vec<u8>, String> {
    let response = client
        .paste_request()
        .send()
        .promise
        .await
        .map_err(|e| e.to_string())?;
    Ok(response.get().unwrap().get_data().unwrap().to_vec())
}

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
