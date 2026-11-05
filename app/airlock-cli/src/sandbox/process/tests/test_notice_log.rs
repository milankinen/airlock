//! Tests of the notice log of the sandbox over the CLI server. The tests use
//! a real Unix socket and the real browser bridge. Only the guest is fake.

use std::time::Duration;

use crate::messages;
use crate::rpc::browser::{Browser, RateLimit};
use crate::sandbox::NoticeReader;
use crate::test_cfg::block_on_local;
use crate::test_cfg::cli_server::{CliClient, CliSandbox, LoopbackGuest};

/// Test that the clients get all notices of the sandbox from two sources,
/// in order and with growing numbers, at each read. A client that connects
/// late must also see the warnings of the start, and `airlock start` finds
/// its new notices by their numbers.
///   1. Add the config change notice as the start work does, and let the
///      browser refuse a page
///   2. Read the notices two times and check that both reads give the
///      same two notices with the numbers 1 and 2
///   3. Let the browser refuse the same page again and check that the log
///      did not change
///   4. Let the browser refuse an other page, read again and check that
///      the old notices did not change and the new one has number 3
#[test]
fn clients_read_all_notices_of_start_and_browser_with_growing_numbers() {
    block_on_local(async {
        let sandbox = CliSandbox::start("", false, LoopbackGuest::client()).await;
        // No opener: the test must not open a browser on the host.
        let limit = RateLimit {
            max_opens: 10,
            window: Duration::from_mins(1),
        };
        let browser = Browser::with_opener(vec![], None, limit, sandbox.notices.clone());
        sandbox
            .notices
            .push(messages::sandbox_process::CONFIG_CHANGED.to_string());
        assert!(browser.open("not a url").is_err());
        let client = CliClient::connect(&sandbox.sock).await;
        let reader = NoticeReader::from_client(client.service.clone());

        let first = reader.read().await;
        assert_eq!(first, reader.read().await);
        let seqnos: Vec<u64> = first.iter().map(|n| n.seqno).collect();
        assert_eq!(seqnos, [1, 2]);
        assert_eq!(first[0].text, messages::sandbox_process::CONFIG_CHANGED);
        assert!(first[1].text.starts_with("browser: "), "{first:?}");

        assert!(browser.open("not a url").is_err());
        assert_eq!(reader.read().await, first);
        assert!(browser.open("https://example.com/$(id)").is_err());
        let later = reader.read().await;
        assert_eq!(later[..2], first[..]);
        assert_eq!(later.len(), 3);
        assert_eq!(later[2].seqno, 3);
    });
}
