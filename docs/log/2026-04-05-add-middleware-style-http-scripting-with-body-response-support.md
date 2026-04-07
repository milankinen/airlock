# Add middleware-style HTTP scripting with body/response support

Reworked the Lua scripting engine from a simple request filter to a full
middleware pipeline with request + response interception.

### Middleware architecture

Scripts compose as layers around the actual HTTP send, like web framework
middleware. Each script receives `req` as a function parameter (not a
global — prevents races on concurrent requests). Scripts can:

- Inspect/modify request (method, path, headers, body)
- Call `req:send()` to forward and get the response (async, yields)
- Inspect/modify response (status, headers, body)
- Call `req:deny()` to reject the request
- If `send()` is not called, it's called implicitly after script ends

### Body userdata

New `Body` type wrapping `Bytes` with:
- `body:text()` — raw bytes as Lua string
- `body:json()` — parse JSON → Lua table (via mlua serde)
- `#body` — byte length
- `FromLua` coercion: string → bytes, table → JSON, Body → clone, nil → empty
- `req:setBody()` / `res:setBody()` accept any coercible value

### Implementation

- `State` wraps `Rc<RefCell<Option<(Parts, Body)>>>` — Lua field
  getters/setters read/write hyper request parts directly
- `RespState` wraps `Rc<RefCell<Option<Response>>>` — same for response
- `with_req()`/`with_resp()` helpers eliminate Option unwrap boilerplate
- `header(key)` / `setHeader(key, val)` for single-header access
- Scripts wrapped in `function(req)..end` to make `req` a local parameter
- `CompiledMiddleware` uses `Rc<Inner>` for cloneability across requests
- mlua `async` + `serialize` features for async methods + JSON support

### Config rename

`[[network.rules]]` → `[[network.middleware]]` to reflect the new role.
