use crate::supervisor_capnp::{byte_stream, data_frame};
use bytes::Bytes;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

/// Server-side ByteStream: serves `read()` calls backed by a boxed AsyncRead.
/// Converts any local data source into a capnp ByteStream capability.
///
/// ```ignore
/// let client: byte_stream::Client = OutputStream::from(tokio::io::stdin()).into();
/// ```
pub struct OutputStream {
    reader: std::cell::RefCell<Pin<Box<dyn AsyncRead + Unpin>>>,
}

impl<R: AsyncRead + Unpin + 'static> From<R> for OutputStream {
    fn from(reader: R) -> Self {
        Self {
            reader: std::cell::RefCell::new(Box::pin(reader)),
        }
    }
}

impl From<OutputStream> for byte_stream::Client {
    fn from(output: OutputStream) -> Self {
        capnp_rpc::new_client(output)
    }
}

impl byte_stream::Server for OutputStream {
    async fn read(
        self: Rc<Self>,
        _params: byte_stream::ReadParams,
        mut results: byte_stream::ReadResults,
    ) -> Result<(), capnp::Error> {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 4096];
        let mut reader = self.reader.borrow_mut();
        match reader.read(&mut buf).await {
            Ok(0) => results.get().init_frame().set_eof(()),
            Ok(n) => results.get().init_frame().set_data(&buf[..n]),
            Err(e) => results.get().init_frame().set_err(&format!("{e}")),
        }
        Ok(())
    }
}

/// Client-side ByteStream reader. Implements `AsyncRead` so it can be used
/// with `tokio::io::copy`, `BufReader`, etc. Convert to `Stream` if needed
/// via `tokio_util::io::ReaderStream`.
///
/// ```ignore
/// let mut reader = InputStream::from(byte_stream_client);
/// tokio::io::copy(&mut reader, &mut writer).await?;
/// ```
pub struct InputStream {
    client: byte_stream::Client,
    buf: Vec<u8>,
    buf_pos: usize,
    pending: Option<Pin<Box<dyn std::future::Future<Output = Result<Option<Bytes>, std::io::Error>>>>>,
    done: bool,
}

impl From<byte_stream::Client> for InputStream {
    fn from(client: byte_stream::Client) -> Self {
        Self {
            client,
            buf: Vec::new(),
            buf_pos: 0,
            pending: None,
            done: false,
        }
    }
}

fn fetch_frame(
    client: byte_stream::Client,
) -> Pin<Box<dyn std::future::Future<Output = Result<Option<Bytes>, std::io::Error>>>> {
    Box::pin(async move {
        let response = client
            .read_request()
            .send()
            .promise
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let frame = response
            .get()
            .and_then(|r| r.get_frame())
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        match frame.which() {
            Ok(data_frame::Eof(())) => Ok(None),
            Ok(data_frame::Data(Ok(bytes))) => Ok(Some(Bytes::copy_from_slice(bytes))),
            Ok(data_frame::Err(Ok(msg))) => {
                Err(std::io::Error::other(msg.to_str().unwrap_or("remote error")))
            }
            _ => Err(std::io::Error::other("invalid frame")),
        }
    })
}

impl AsyncRead for InputStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // Serve from buffer first
        if self.buf_pos < self.buf.len() {
            let remaining = &self.buf[self.buf_pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.buf_pos += n;
            if self.buf_pos >= self.buf.len() {
                self.buf.clear();
                self.buf_pos = 0;
            }
            return Poll::Ready(Ok(()));
        }

        if self.done {
            return Poll::Ready(Ok(()));
        }

        // Start fetch if not already pending
        if self.pending.is_none() {
            self.pending = Some(fetch_frame(self.client.clone()));
        }

        // Poll the pending fetch
        let fut = self.pending.as_mut().unwrap();
        match fut.as_mut().poll(cx) {
            Poll::Ready(Ok(Some(bytes))) => {
                self.pending = None;
                let n = bytes.len().min(buf.remaining());
                buf.put_slice(&bytes[..n]);
                if n < bytes.len() {
                    self.buf = bytes[n..].to_vec();
                    self.buf_pos = 0;
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok(None)) => {
                self.pending = None;
                self.done = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                self.pending = None;
                self.done = true;
                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
