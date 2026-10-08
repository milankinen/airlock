use std::cell::{Cell, RefCell};
use std::rc::Rc;

use airlock_common::supervisor_capnp::{browser, clipboard};
use airlock_test_utils::rpc_loopback;

/// The host clipboard, served to the guest over a real RPC connection.
/// Call [`HostClipboard::client`] inside a `LocalSet`.
#[derive(Default)]
pub(crate) struct HostClipboard {
    /// Copies the host accepted, in order.
    pub copies: RefCell<Vec<Vec<u8>>>,
    /// What a paste returns; `None` refuses the paste.
    pub contents: RefCell<Option<Vec<u8>>>,
    /// How many upcoming copies the host refuses.
    pub refuse_copies: Cell<usize>,
}

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
    pub fn client(self: &Rc<Self>) -> clipboard::Client {
        let server: clipboard::Client = capnp_rpc::new_client(ClipboardServer(self.clone()));
        rpc_loopback(server.client)
    }

    pub fn copied(&self) -> Vec<String> {
        self.copies
            .borrow()
            .iter()
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect()
    }
}

/// The host browser, served to the guest over a real RPC connection.
/// Call [`HostBrowser::client`] inside a `LocalSet`.
#[derive(Default)]
pub(crate) struct HostBrowser {
    /// URLs the host opened, in order.
    pub opened: RefCell<Vec<String>>,
    /// How many upcoming opens the host refuses.
    pub refuse_opens: Cell<usize>,
}

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
    pub fn client(self: &Rc<Self>) -> browser::Client {
        let server: browser::Client = capnp_rpc::new_client(BrowserServer(self.clone()));
        rpc_loopback(server.client)
    }
}
