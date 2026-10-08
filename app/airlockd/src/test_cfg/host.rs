//! Fake host services (clipboard and browser) for the guest bridges.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use airlock_common::supervisor_capnp::{browser, clipboard};
use airlock_test_utils::rpc_loopback;

/// A fake host clipboard that the guest uses through a real RPC connection.
/// Call [`HostClipboard::client`] in a `LocalSet`.
#[derive(Default)]
pub(crate) struct HostClipboard {
    /// Copies that the host accepted, in order.
    pub copies: RefCell<Vec<Vec<u8>>>,
    /// The data that a paste returns. `None` refuses the paste.
    pub contents: RefCell<Option<Vec<u8>>>,
    /// The number of next copies that the host refuses.
    pub refuse_copies: Cell<usize>,
}

/// RPC server of [`HostClipboard`].
struct ClipboardServer(Rc<HostClipboard>);

impl clipboard::Server for ClipboardServer {
    async fn copy(
        self: Rc<Self>,
        params: clipboard::CopyParams,
        _results: clipboard::CopyResults,
    ) -> Result<(), capnp::Error> {
        let data = params.get()?.get_data()?.to_vec();
        let refuse = self.0.refuse_copies.get();
        if refuse > 0 {
            self.0.refuse_copies.set(refuse - 1);
            return Err(capnp::Error::failed("copy refused".into()));
        }
        self.0.copies.borrow_mut().push(data);
        Ok(())
    }

    async fn paste(
        self: Rc<Self>,
        _params: clipboard::PasteParams,
        mut results: clipboard::PasteResults,
    ) -> Result<(), capnp::Error> {
        let contents = self.0.contents.borrow().clone();
        let data = contents.ok_or_else(|| capnp::Error::failed("paste refused".into()))?;
        results.get().set_data(&data);
        Ok(())
    }
}

impl HostClipboard {
    /// Return an RPC client for this clipboard.
    pub fn client(self: &Rc<Self>) -> clipboard::Client {
        let server: clipboard::Client = capnp_rpc::new_client(ClipboardServer(self.clone()));
        rpc_loopback(server.client)
    }

    /// Return the accepted copies as text.
    pub fn copied(&self) -> Vec<String> {
        self.copies
            .borrow()
            .iter()
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect()
    }
}

/// A fake host browser that the guest uses through a real RPC connection.
/// Call [`HostBrowser::client`] in a `LocalSet`.
#[derive(Default)]
pub(crate) struct HostBrowser {
    /// URLs that the host opened, in order.
    pub opened: RefCell<Vec<String>>,
    /// The number of next opens that the host refuses.
    pub refuse_opens: Cell<usize>,
}

/// RPC server of [`HostBrowser`].
struct BrowserServer(Rc<HostBrowser>);

impl browser::Server for BrowserServer {
    async fn open(
        self: Rc<Self>,
        params: browser::OpenParams,
        _results: browser::OpenResults,
    ) -> Result<(), capnp::Error> {
        let url = params.get()?.get_url()?.to_str()?.to_string();
        let refuse = self.0.refuse_opens.get();
        if refuse > 0 {
            self.0.refuse_opens.set(refuse - 1);
            return Err(capnp::Error::failed("open refused".into()));
        }
        self.0.opened.borrow_mut().push(url);
        Ok(())
    }
}

impl HostBrowser {
    /// Return an RPC client for this browser.
    pub fn client(self: &Rc<Self>) -> browser::Client {
        let server: browser::Client = capnp_rpc::new_client(BrowserServer(self.clone()));
        rpc_loopback(server.client)
    }
}
