# Network services

A network service lets an agent sign in inside the sandbox, while the
real tokens stay on the host. The sandbox gets only surrogates. The
network layer knows no OAuth. It gives the traffic of the owned hosts
to the service through an interceptor.

| Service     | Token hosts           | API hosts           |
|-------------|-----------------------|---------------------|
| `anthropic` | `platform.claude.com` | `api.anthropic.com` |
| `openai`    | `auth.openai.com`     | `chatgpt.com`       |

## Owned hosts

- airlock always intercepts an owned host. It is never passthrough.
- A TLS stream to an owned host that is not HTTP is closed before the
  upstream connect. There is no raw relay.
- Monitor events and Lua middleware run before the service. They see
  only surrogates.

## Sign-in

The browser bridge opens the sign-in page on the host:

1. The agent runs `xdg-open` or `$BROWSER`. Both are a guest shim in
   `/run/airlock/bin`, a tmpfs for each boot.
2. The shim writes the URL to a FIFO. `airlockd` calls `Browser.open`.
3. The host checks the URL: https, no userinfo, port or fragment, and a
   limit of 5 opens per minute.
4. The service checks the page: the authorize host and path,
   `response_type=code`, PKCE S256, no `prompt=none`, and one loopback
   `redirect_uri` on an allowed port and path.
5. The host binds the callback port and opens the page.

The callback ports are 32768-60999 for Anthropic, and 1455 or 1457 for
OpenAI. The callback forward accepts only `GET`. It removes cookies and
`Authorization`. It sends only some answer headers to the browser. It closes
60 seconds after the code, or after 10 minutes without a code.

The callback forward and the Codex device poll change the real
authorization code to a surrogate code `airlock-code-...`. A surrogate
code works once, for 10 minutes, and only for its service and channel.

Claude's manual code paste brings the real code into the sandbox. The
exchange goes upstream only if its PKCE `code_verifier` matches a page
that the host opened in the last 10 minutes.

## Token hosts

Only some routes are allowed, for example `POST /v1/oauth/token`.
Other routes get a local 403. The proxy forwards an allowed route with
its own constant path and no query.

The token engine reads each token answer:

- Each string that has a known token format gets a surrogate. The other
  fields go to the agent unchanged.
- A string under a secret-like key (`token`, `secret`, `key`, `auth`,
  and similar) with no known format refuses the answer (local 502).
- Surrogates have the format of the provider: `sk-ant-oat01-airlock-...`
  for Anthropic, unsigned JWTs with the real claims for OpenAI. Each has
  48 random bytes.

The refresh relay builds the upstream body itself, with the stored
client ID and the real refresh token. Nothing from the guest body goes
upstream. The upstream call runs in a separate task, so a dropped guest
request does not lose rotated tokens.

A revoke with a surrogate removes the sign-in for all sandboxes and
revokes the real tokens upstream. A new sign-in replaces older sign-ins
with the same account, client and scopes.

## API hosts

- The proxy changes only `Authorization: Bearer <token>` and
  `x-api-key`, and only if the whole value is a surrogate of the
  service. Bodies and other headers do not change. Upstreams reflect
  headers, so a swap in all headers would show the real token.
- Strict credentials: the value must be an access or API-key surrogate
  of the service, or an injected masked secret. Other values get 401
  `airlock_foreign_credential`. Thus a sandbox cannot put its own token
  into the shared credential file.

The answer scan reads each API answer while it streams:

- Requests go upstream with `Accept-Encoding: identity`. A compressed
  answer gets a 502.
- The scan searches for each real token of the store and each injected
  secret. Outside `text/event-stream`, it also searches for the token
  shapes of the provider.
- The scan holds back the end of a chunk that can start a token, so it
  also finds a split token.
- A JSON answer waits until 64 KiB are scanned, so a hit gives a clean
  502. Other answers stream at once. A hit there ends the stream with an
  error, and the chunk with the token is not sent.

The scan does not read WebSocket frames, and it does not find encoded
tokens (base64, JSON escapes, URL encoding).

## Token store

The store is an LMDB database in `~/.airlock/db/`. Each service has
three keys:

- `<service>.secrets`: all sign-ins as one document, sealed with
  ChaCha20-Poly1305. The key comes from the vault field
  `service_store_key`.
- `<service>.meta`: plain data for `airlock show` (account, scopes,
  time). No tokens.
- `<service>.generation`: a counter. A reader decrypts again only when
  the counter changed, so all processes see sign-outs at once.

If the vault is `disabled` or the key cannot be read, the service is not
available, and its hosts are denied. Otherwise the agent could sign in
without airlock and keep the real tokens in the sandbox.
