//! Local HTTP servers and a hyper executor for tests.

use std::future::Future;
use std::net::SocketAddr;

use axum::Router;
use tokio::net::TcpListener;

/// Serve `app` on an ephemeral `127.0.0.1` port until the runtime ends.
pub async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Serve `app` on an ephemeral `127.0.0.1` port until the returned sender
/// is used or dropped.
pub async fn serve_with_shutdown(app: Router) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, tx)
}

/// Serve `app` over HTTP/1.1 and HTTP/2 on one accepted stream.
pub async fn serve_connection<S>(stream: S, app: Router)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |req| {
        let mut app = app.clone();
        async move {
            use tower::Service;
            app.call(req).await.map_err(|e| match e {})
        }
    });
    let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
        .await;
}

/// A hyper executor that spawns onto the current `LocalSet`, for clients
/// over `!Send` streams.
#[derive(Clone, Copy)]
pub struct LocalExec;

impl<F> hyper::rt::Executor<F> for LocalExec
where
    F: Future + 'static,
{
    fn execute(&self, fut: F) {
        tokio::task::spawn_local(async move {
            fut.await;
        });
    }
}
