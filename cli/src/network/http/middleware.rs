//! Lua-based HTTP middleware: compile scripts, then run them in a chain
//! around each proxied HTTP request.
//!
//! Each script receives a `req` userdata with fields like `method`, `path`,
//! `headers` and methods like `body()`, `setBody()`, `send()`, `deny()`.
//! Scripts can inspect and modify the request before forwarding, or
//! inspect/modify the response after.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use bytes::Bytes;
use http_body_util::{BodyExt, Either, Full};
use hyper::body::Incoming;
use mlua::{Function, Lua, UserData, UserDataFields, UserDataMethods};
use tracing::trace;

use crate::network::{matchers, middleware};

type RequestBody = Either<Incoming, Full<Bytes>>;

type MiddlewareNext = Box<dyn FnOnce(hyper::http::request::Parts, RequestBody) -> NextFuture>;
type NextFuture =
    Pin<Box<dyn Future<Output = mlua::Result<hyper::Response<Either<Incoming, Full<Bytes>>>>>>>;

/// A compiled Lua middleware script, ready to be invoked per-request.
#[derive(Clone)]
pub struct CompiledMiddleware(Rc<Inner>);

struct Inner {
    lua: Lua,
    func: Function,
}

/// Compile a Lua middleware script. The script is wrapped in a closure so
/// `req` is a local parameter rather than a global, preventing races.
///
/// `env_vars` maps variable names to their descriptions; values are resolved
/// from the host environment and exposed to the script as the `env` global
/// table (nil for any variable not set on the host).
pub fn compile(
    script: &str,
    env_vars: &BTreeMap<String, String>,
    log: middleware::LogFn,
) -> anyhow::Result<CompiledMiddleware> {
    let lua = Lua::new();
    middleware::sandbox(&lua)?;

    let log_fn = lua.create_function(move |_, msg: String| {
        log(&msg);
        Ok(())
    })?;
    lua.globals().set("log", log_fn)?;

    // Expose declared env vars as the `env` global table.
    // Values are subst templates expanded from the host environment.
    // Any template that references an undefined host variable resolves to nil.
    let host_env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let env_table = lua.create_table()?;
    for (key, template) in env_vars {
        let value = subst::substitute(template, &host_env).ok();
        env_table.set(key.as_str(), value)?;
    }
    lua.globals().set("env", env_table)?;

    // Wrap the script in a function(req) so `req` is a local parameter,
    // not a global. This prevents races when concurrent requests share
    // the same Lua instance.
    let wrapped = format!("return function(req)\n{script}\nend");
    let func: Function = lua
        .load(&wrapped)
        .set_name("middleware")
        .eval()
        .map_err(|e| anyhow::anyhow!("failed to compile middleware '{script}': {e}"))?;

    Ok(CompiledMiddleware(Rc::new(Inner { lua, func })))
}

#[derive(Debug, thiserror::Error)]
#[error("denied by network rules")]
pub struct Denied;

/// Check if an mlua error (possibly nested in CallbackError) contains Denied.
fn is_denied(e: &mlua::Error) -> bool {
    match e {
        mlua::Error::ExternalError(e) => e.downcast_ref::<Denied>().is_some(),
        mlua::Error::CallbackError { cause, .. } => is_denied(cause),
        _ => false,
    }
}

/// Run all HTTP middleware layers around the send function.
pub async fn run<F, Fut>(
    req: hyper::Request<Incoming>,
    middleware: &[CompiledMiddleware],
    send: F,
) -> anyhow::Result<hyper::Response<Either<Incoming, Full<Bytes>>>>
where
    F: FnOnce(hyper::Request<RequestBody>) -> Fut + 'static,
    Fut: Future<Output = anyhow::Result<hyper::Response<Incoming>>> + 'static,
{
    if middleware.is_empty() {
        return send(req.map(Either::Left))
            .await
            .map(|r| r.map(Either::Left));
    }

    // Innermost: the actual hyper send
    let mut next: MiddlewareNext = Box::new(move |parts, body| -> NextFuture {
        Box::pin(async move {
            let req = hyper::Request::from_parts(parts, body);
            send(req)
                .await
                .map(|r| r.map(Either::Left))
                .map_err(|e| mlua::Error::runtime(format!("{e}")))
        })
    });

    // Wrap with each middleware layer (innermost first)
    for m in middleware.iter().rev() {
        let inner = next;
        let m = m.0.clone();

        next = Box::new(move |parts, body| -> NextFuture {
            Box::pin(async move {
                let state = State {
                    req: Rc::new(RefCell::new(Some((parts, body)))),
                    next: Rc::new(RefCell::new(Some(inner))),
                    resp: Rc::new(RefCell::new(None)),
                };

                trace!("running http middleware '{:?}'", m.func);
                let thread = m.lua.create_thread(m.func.clone())?;
                // Pass state as the function argument (not a global)
                let result = thread.into_async::<()>(state.clone())?.await;

                match result {
                    Ok(()) => {}
                    Err(e) if is_denied(&e) => {
                        return Err(mlua::Error::external(Denied));
                    }
                    Err(e) => {
                        return Err(mlua::Error::runtime(format!("middleware error: {e}")));
                    }
                }

                // If script called send(), response is stored in the linked RespState
                if let Some(resp_ref) = state.resp.borrow_mut().take()
                    && let Some(resp) = resp_ref.borrow_mut().take()
                {
                    return Ok(resp);
                }

                // Script didn't call send() — do it implicitly
                trace!("middleware did not call send(), sending implicitly!");
                let next = state
                    .next
                    .borrow_mut()
                    .take()
                    .ok_or_else(|| mlua::Error::runtime("send already consumed"))?;
                let (parts, body) = state
                    .req
                    .borrow_mut()
                    .take()
                    .ok_or_else(|| mlua::Error::runtime("request already consumed"))?;
                next(parts, body).await
            })
        });
    }

    // Kick off the chain
    let (parts, body) = req.into_parts();
    let result = next(parts, Either::Left(body)).await;
    match result {
        Ok(resp) => Ok(resp),
        Err(ref e) if is_denied(e) => Ok(hyper::Response::builder()
            .status(403)
            .body(Either::Right(Full::new(Bytes::from(
                "Denied by network rules\n",
            ))))
            .unwrap()),
        Err(e) => anyhow::bail!("{e}"),
    }
}

type ResponseRef = Rc<RefCell<Option<hyper::Response<Either<Incoming, Full<Bytes>>>>>>;

/// Shared state between the middleware runner and the Lua UserData methods.
#[derive(Clone)]
struct State {
    req: Rc<RefCell<Option<(hyper::http::request::Parts, RequestBody)>>>,
    next: Rc<RefCell<Option<MiddlewareNext>>>,
    resp: Rc<RefCell<Option<ResponseRef>>>,
}

impl State {
    fn with_req<T>(
        &self,
        f: impl FnOnce(&hyper::http::request::Parts, &RequestBody) -> mlua::Result<T>,
    ) -> mlua::Result<T> {
        let req = self.req.borrow();
        let (parts, body) = req
            .as_ref()
            .ok_or_else(|| mlua::Error::runtime("request consumed"))?;
        f(parts, body)
    }

    fn with_req_mut<T>(
        &self,
        f: impl FnOnce(&mut hyper::http::request::Parts, &mut RequestBody) -> mlua::Result<T>,
    ) -> mlua::Result<T> {
        let mut req = self.req.borrow_mut();
        let (parts, body) = req
            .as_mut()
            .ok_or_else(|| mlua::Error::runtime("request consumed"))?;
        f(parts, body)
    }
}

impl UserData for State {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("method", |_, this| {
            this.with_req(|p, _| Ok(p.method.to_string()))
        });
        fields.add_field_method_get("path", |_, this| {
            this.with_req(|p, _| {
                Ok(p.uri
                    .path_and_query()
                    .map_or_else(|| "/".to_string(), ToString::to_string))
            })
        });
        fields.add_field_method_set("path", |_, this, val: String| {
            this.with_req_mut(|p, _| {
                p.uri = val
                    .parse()
                    .map_err(|e| mlua::Error::runtime(format!("invalid path: {e}")))?;
                Ok(())
            })
        });
        fields.add_field_method_get("headers", |lua, this| {
            this.with_req(|p, _| {
                let table = lua.create_table()?;
                for (k, v) in &p.headers {
                    table.set(k.as_str(), v.to_str().unwrap_or(""))?;
                }
                Ok(table)
            })
        });
        fields.add_field_method_set("headers", |_, this, table: mlua::Table| {
            this.with_req_mut(|p, _| {
                p.headers.clear();
                for pair in table.pairs::<String, String>() {
                    let (k, v) = pair?;
                    let name = hyper::header::HeaderName::from_bytes(k.as_bytes())
                        .map_err(|e| mlua::Error::runtime(format!("invalid header name: {e}")))?;
                    let value = hyper::header::HeaderValue::from_str(&v)
                        .map_err(|e| mlua::Error::runtime(format!("invalid header value: {e}")))?;
                    p.headers.append(name, value);
                }
                Ok(())
            })
        });
        fields.add_field_method_get("host", |_, this| {
            this.with_req(|p, _| {
                let host = p
                    .uri
                    .host()
                    .or_else(|| {
                        p.headers
                            .get("host")
                            .and_then(|v| v.to_str().ok())
                            .map(|h| h.split(':').next().unwrap_or(h))
                    })
                    .unwrap_or("");
                Ok(host.to_string())
            })
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("header", |_, this, key: String| {
            this.with_req(|p, _| {
                Ok(p.headers
                    .get(key.as_str())
                    .and_then(|v| v.to_str().ok())
                    .map(String::from))
            })
        });
        methods.add_method("setHeader", |_, this, (key, value): (String, String)| {
            this.with_req_mut(|p, _| {
                let name = hyper::header::HeaderName::from_bytes(key.as_bytes())
                    .map_err(|e| mlua::Error::runtime(format!("invalid header name: {e}")))?;
                let val = hyper::header::HeaderValue::from_str(&value)
                    .map_err(|e| mlua::Error::runtime(format!("invalid header value: {e}")))?;
                p.headers.insert(name, val);
                Ok(())
            })
        });
        methods.add_method("hostMatches", |_, this, pattern: String| {
            this.with_req(|p, _| {
                let host = p
                    .uri
                    .host()
                    .or_else(|| {
                        p.headers
                            .get("host")
                            .and_then(|v| v.to_str().ok())
                            .map(|h| h.split(':').next().unwrap_or(h))
                    })
                    .unwrap_or("");
                Ok(matchers::host_matches(host, &pattern))
            })
        });

        methods.add_method("deny", |_, _, ()| -> mlua::Result<()> {
            Err(mlua::Error::external(Denied))
        });

        // body() — collect the streaming body, returns Body userdata
        methods.add_async_method("body", |_, this, ()| async move {
            // Take body out of RefCell before awaiting (can't hold borrow across await)
            let body = {
                let mut req = this.req.borrow_mut();
                let (_, body) = req
                    .as_mut()
                    .ok_or_else(|| mlua::Error::runtime("request consumed"))?;
                let mut taken = Either::Right(Full::new(Bytes::new()));
                std::mem::swap(body, &mut taken);
                taken
            };
            let collected = body
                .collect()
                .await
                .map_err(|e| mlua::Error::runtime(format!("read body: {e}")))?
                .to_bytes();
            // Put the buffered body back
            if let Some((_, body)) = this.req.borrow_mut().as_mut() {
                *body = Either::Right(Full::new(collected.clone()));
            }
            Ok(super::body::Body(collected))
        });

        // setBody(val) — set body from string, table (→ JSON), or Body
        // Also updates Content-Length header.
        methods.add_method("setBody", |_, this, val: super::body::Body| {
            this.with_req_mut(|parts, body| {
                let len = val.0.len();
                *body = Either::Right(Full::new(val.0));
                parts
                    .headers
                    .insert("content-length", len.to_string().parse().unwrap());
                Ok(())
            })
        });

        // send() — forward through the middleware chain, returns response userdata
        methods.add_async_method("send", |_, this, ()| async move {
            let next = this
                .next
                .borrow_mut()
                .take()
                .ok_or_else(|| mlua::Error::runtime("send() already called"))?;
            let (parts, body) = this
                .req
                .borrow_mut()
                .take()
                .ok_or_else(|| mlua::Error::runtime("request already consumed"))?;
            let resp = next(parts, body).await?;
            let resp_state = RespState {
                inner: Rc::new(RefCell::new(Some(resp))),
            };
            // Link the response state so we can retrieve it after script ends
            *this.resp.borrow_mut() = Some(resp_state.inner.clone());
            Ok(resp_state)
        });
    }
}

type HttpResponse = hyper::Response<Either<Incoming, Full<Bytes>>>;

/// Response userdata — wraps the hyper response parts directly.
#[derive(Clone)]
struct RespState {
    inner: Rc<RefCell<Option<HttpResponse>>>,
}

impl RespState {
    fn with_resp<T>(&self, f: impl FnOnce(&HttpResponse) -> mlua::Result<T>) -> mlua::Result<T> {
        let resp = self.inner.borrow();
        f(resp
            .as_ref()
            .ok_or_else(|| mlua::Error::runtime("response consumed"))?)
    }

    fn with_resp_mut<T>(
        &self,
        f: impl FnOnce(&mut HttpResponse) -> mlua::Result<T>,
    ) -> mlua::Result<T> {
        let mut resp = self.inner.borrow_mut();
        f(resp
            .as_mut()
            .ok_or_else(|| mlua::Error::runtime("response consumed"))?)
    }
}

impl UserData for RespState {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("status", |_, this| {
            this.with_resp(|r| Ok(r.status().as_u16()))
        });
        fields.add_field_method_set("status", |_, this, val: u16| {
            this.with_resp_mut(|r| {
                *r.status_mut() = hyper::StatusCode::from_u16(val)
                    .map_err(|e| mlua::Error::runtime(format!("invalid status: {e}")))?;
                Ok(())
            })
        });
        fields.add_field_method_get("headers", |lua, this| {
            this.with_resp(|r| {
                let table = lua.create_table()?;
                for (k, v) in r.headers() {
                    table.set(k.as_str(), v.to_str().unwrap_or(""))?;
                }
                Ok(table)
            })
        });
        fields.add_field_method_set("headers", |_, this, table: mlua::Table| {
            this.with_resp_mut(|r| {
                r.headers_mut().clear();
                for pair in table.pairs::<String, String>() {
                    let (k, v) = pair?;
                    let name = hyper::header::HeaderName::from_bytes(k.as_bytes())
                        .map_err(|e| mlua::Error::runtime(format!("invalid header name: {e}")))?;
                    let value = hyper::header::HeaderValue::from_str(&v)
                        .map_err(|e| mlua::Error::runtime(format!("invalid header value: {e}")))?;
                    r.headers_mut().append(name, value);
                }
                Ok(())
            })
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("header", |_, this, key: String| {
            this.with_resp(|r| {
                Ok(r.headers()
                    .get(key.as_str())
                    .and_then(|v| v.to_str().ok())
                    .map(String::from))
            })
        });
        methods.add_method("setHeader", |_, this, (key, value): (String, String)| {
            this.with_resp_mut(|r| {
                let name = hyper::header::HeaderName::from_bytes(key.as_bytes())
                    .map_err(|e| mlua::Error::runtime(format!("invalid header name: {e}")))?;
                let val = hyper::header::HeaderValue::from_str(&value)
                    .map_err(|e| mlua::Error::runtime(format!("invalid header value: {e}")))?;
                r.headers_mut().insert(name, val);
                Ok(())
            })
        });

        // body() — collect the response body, returns Body userdata
        methods.add_async_method("body", |_, this, ()| async move {
            let body = {
                let mut resp = this.inner.borrow_mut();
                let resp = resp
                    .as_mut()
                    .ok_or_else(|| mlua::Error::runtime("response consumed"))?;
                let mut taken = Either::Right(Full::new(Bytes::new()));
                std::mem::swap(resp.body_mut(), &mut taken);
                taken
            };
            let collected = body
                .collect()
                .await
                .map_err(|e| mlua::Error::runtime(format!("read body: {e}")))?
                .to_bytes();
            if let Some(resp) = this.inner.borrow_mut().as_mut() {
                *resp.body_mut() = Either::Right(Full::new(collected.clone()));
            }
            Ok(super::body::Body(collected))
        });

        // setBody(val) — set body from string, table (→ JSON), or Body
        // Also updates Content-Length header.
        methods.add_method("setBody", |_, this, val: super::body::Body| {
            this.with_resp_mut(|r| {
                let len = val.0.len();
                *r.body_mut() = Either::Right(Full::new(val.0));
                r.headers_mut()
                    .insert("content-length", len.to_string().parse().unwrap());
                Ok(())
            })
        });
    }
}
