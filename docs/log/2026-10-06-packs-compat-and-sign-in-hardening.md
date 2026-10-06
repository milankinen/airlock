# Keep upgrades compatible and harden packs and sign-ins

This commit changes two things relative to the previous commit (packs,
setup wizard, agent sign-ins via the host). It keeps a user who upgrades
from the released `main` without config changes on the old behaviour.
It also hardens the new install boot, `airlock rm` and the network
services. The services get a generic token design that does not need a
code change for each agent update.

## Compatibility with the released version

- Legacy list presets resolve as released. `config/presets/python.toml`
  (the `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` and `PIP_CERT` env) and
  `debian.toml` (`ports.ubuntu.com`) are back to the released bytes. The
  edited content stays in the `@1` packs only. With the edited file, the
  python list set a CA path that does not exist on Fedora/SUSE images.
  The golden oracle `config/tests/golden/legacy-presets.json` is
  regenerated from the released files, so it pins the released
  behaviour again. 11 names were released; `docker` stays as an extra
  list name.
- `presets: null` (YAML `presets:` with no value, JSON `null`) is
  treated as absent again, as released and as `merge_json` treats other
  nulls. Other invalid values are still config errors
  (`tests/cli/config_loading.bats`).
- The services token store (not in any release) changes its layout. The
  databases `services.grants` and `services.lookups` of pre-release
  builds are emptied when a process first uses the store. heed 0.22
  cannot delete a named database, so the names stay without records
  (`Db::empty_database`). Users of pre-release builds must sign in
  again.
- `InstallState` gets `ran_session`. A state file without the field
  counts as "a session ran" (see below), which is the safe default.

## Install boot

- The install boot network is public-only (`Network::public_only`).
  `resolve_target` denies `localhost`, `*.localhost` and non-public IP
  literals. `tcp::dial` resolves the name itself and connects only to
  the public addresses it got. Because the check is on the dialed
  addresses, names like `127.1` and DNS rebinding cannot bypass it.
  `target::is_public_ip` refuses loopback, private, link-local (cloud
  metadata included), CGNAT, broadcast, multicast, reserved, ULA and
  site-local addresses. It checks the IPv4 address inside IPv4-mapped,
  IPv4-compatible, NAT64 and 6to4 IPv6 addresses. Normal sessions do not
  change. The install question text now says that local and private
  addresses are blocked.
- A retry of an `unconfirmed` or `failed` install no longer runs without
  a question when a normal session ran on the disk after the install.
  That session can have left code that the install boot then runs. The
  plan gives `Why::New` instead of `Why::Retry`, so the added-tools
  question is asked (or `--yes` / a terminal is needed). The flag is
  cleared when an install boot saves its first record and set before the
  next session.

## `airlock rm` and symlinks

- `airlock rm` checks `.airlock` with `symlink_metadata`. A symlinked
  `.airlock` is only unlinked; the prompt and the log name the target,
  and the target is never followed or deleted. The sandbox-only removal
  checks again for a symlink immediately before it deletes, and stops
  if it finds one.
- `project::lock_if_idle` does not follow a symlinked `sandbox` dir
  (it is treated as missing). It opens `lock` with
  `O_NOFOLLOW | O_NONBLOCK` and accepts only a regular file. A planted
  link or FIFO cannot make `rm` take another sandbox's lock or hang.
- The "is a symbolic link" errors now point to `airlock rm` for removing
  just the link.

## Network services: transport

- Owned hosts never fall back to a raw relay. A TLS stream to an owned
  host that does not parse as HTTP is closed before any upstream connect
  (`network/server.rs`). All fail-closed handling lives in the HTTP
  interceptor, and raw bytes would bypass it.
- The relay's body type is widened so that an interceptor can wrap a
  streamed answer: `ResponseBody = Either<StreamBody, Full<Bytes>>` with
  `StreamBody = Either<Incoming, UnsyncBoxBody<Bytes, BoxError>>`
  (`network/http.rs`, Lua middleware types follow).

## Network services: sign-in

- OAuth client and scopes come from the sign-in. The browser check
  (`sign_in.rs`) accepts any single non-empty `client_id` and any scope
  set (one `scope`, at least one scope, no duplicates, each an RFC 6749
  scope token). It still requires `response_type=code`, S256 with one
  non-empty challenge, no `response_mode`, no `prompt=none`, the
  loopback redirect on the agent's callback ports and path, and the
  authorize host and path. Reason: Claude Code has a second OAuth client
  for Console accounts (`41077d10-…`, scopes `user:profile
  user:inference`) that a single `client_id` constant refused.
- The code exchange must name a `client_id`, and the grant stores it.
  Refresh and revoke send the stored client id, never the guest's value,
  so the guest cannot point a refresh at another client. Accepting any
  client does not widen what the sandbox gets. The sandbox never sees a
  real code or token, the surrogate code binds the exchange to a
  callback this process forwarded, and the provider decides what the
  client may do.
- A new sign-in replaces older grants with the same account, client id
  and scopes. The client is now part of the identity.
- Claude's manual code paste is bound to a page that this process
  opened. The user pastes the real code, so there is no surrogate to
  redeem. When the browser bridge allows a page and its callback port
  binds, the page's PKCE `code_challenge` is recorded
  (`PendingCodes::open_page`). This also happens when the host has no
  browser program, so a manual paste over SSH still works. The manual
  exchange (that exact `redirect_uri`) is forwarded only when its
  `code_verifier` hashes to such a challenge: once, within 10 minutes,
  per service, at most 32 pending. Without this binding, any code went
  upstream and a sandbox could plant a grant of another account in the
  shared credentials. Limit: the sandbox still picks the challenge, so
  this needs a visible host page per exchange but does not fully close
  the attack.
- Callback forwards: a forward closes and frees its port 10 minutes
  after it opens if no code comes (60 s after a code, as before). Only
  `Content-Type`, `Content-Length`, `Cache-Control` and a checked
  `Location` of the sandbox's answer reach the browser. No
  `Set-Cookie`, `Refresh` or CORS headers go through.
- Device sign-in (`codex login --device-auth`): `code` inside an
  `error`/`errors` object is an error code, not an authorization code.
  Its value is still checked for token formats. The poll's pending
  answer `{"error":{"code":"deviceauth_authorization_pending"}}` no
  longer gets a 502. The device routes pass the string values of
  `device_auth_id` and `user_code` unchecked, because they can look like
  OpenAI tokens. Their key names still count.

## Network services: token hosts

- Route allowlist. `platform.claude.com`: `POST /v1/oauth/token`,
  `POST /v1/oauth/token/revoke`, `GET /v1/oauth/hello`.
  `auth.openai.com`: `POST /oauth/token`, `POST /oauth/revoke`,
  `POST /api/accounts/deviceauth/{usercode,token}`. All other routes get
  a local 403 `airlock_route_not_allowed`. Claude's preflight is
  `${origin of TOKEN_URL}/v1/oauth/hello`, so it goes to
  `platform.claude.com`. Codex opens `/codex/device` only in the user's
  browser. This was checked in the binaries of Claude Code 2.1.283 and
  codex 0.155.1.
- Allowed routes are matched on the normalized path and forwarded with
  the route's own constant path and no query (`oauth::route_to`). The
  guest's spelling (for example `/x%2f..%2f/v1/oauth/hello`) never goes
  upstream. HTTP/2 keeps its scheme and authority.
- The refresh relay builds the upstream body itself
  (`Provider::refresh_body`): an allowlist of fields with the stored
  client id and the real refresh token. Nothing from the guest's body
  goes upstream. Anthropic sends the stored scopes without
  `org:create_api_key`. OpenAI sends only `client_id`, `grant_type` and
  `refresh_token`, as Codex does. Codex's fallback revoke of the access
  token names no client, as Codex does. The refresh-token revoke sends
  the stored client id.

## Network services: generic token engine (`services/tokens.rs`)

- Each provider has a table of token formats: kind (access, refresh,
  ID, API key), a recognizer (key and value), the surrogate prefix test,
  a minter, and whether the surrogate carries the real token's claims.
  The engine knows no provider. A new field with a known format needs no
  code change. A new secret format fails closed and does not leak.
- In a token answer, every recognized string anywhere gets a surrogate,
  all in one grant. The main token of a kind is the one under the
  answer's own top-level key. Other recognized strings become extra
  tokens of the grant, and their surrogates work too. Only the tokens
  are substituted. The other fields pass as the provider sent them, so
  `refresh_token_expires_in` and `token_type` now reach the agent.
- The standard fields are key-driven. Each table ends with one fallback
  format per field (`access_token`, `refresh_token`, `id_token`) that
  takes any non-empty value. These come after the known shapes, whose
  minters keep Anthropic's prefixes and OpenAI's JWT claims. Fallback
  surrogates: Anthropic `sk-ant-oat01-airlock-` / `sk-ant-ort01-airlock-`
  / `airlock-id-` (Claude Code checks the access prefix), OpenAI
  `airlock-at-` / `airlock-rt-` / `airlock-id-`. Reason: the format of
  OpenAI's real refresh token is not known. A shape guess refused every
  `codex login` in a real run.
- Fail-closed for other secret-like keys. A string under a token-like
  key (also nested below one) that no format recognizes and that is no
  airlock surrogate refuses the whole answer: local 502, nothing
  stored. Token-like keys contain `token`, `secret`, `key`, `cred`,
  `password`, `passwd`, `session`, `jwt`, `bearer`, `cookie` or
  `assertion`, are `code` or `authorization_code`, or have the word
  `auth` (`x_auth`, `authValue`; not `authorization_endpoint`). Exempt
  keys: `token_type`-style metadata, `id`, `uuid`, `*_id`, `*_uuid`.
  Exempt values: values that cannot be a credential (shorter than 16
  characters, UUIDs, numbers, booleans). Without these exemptions a
  real Claude sign-in was refused on `token_uuid`.
- An OAuth answer can carry only access, refresh and ID tokens; an API
  key in it refuses it. `create_api_key` runs the engine non-strictly:
  only API-key formats count, and an OAuth token refuses the answer.
  The other fields of that answer are not known, and a strict walk
  would refuse a field such as `key_id`. Limits stay: 3 keys per grant
  per hour (local 429), at most 8 kept.
- Refresh: each kind in the answer replaces the grant's tokens of that
  kind. Kinds the answer omits stay (Claude keeps the old refresh
  token; Codex's fields are all optional). Opaque main surrogates stay
  the same. Claim-carrying ones (OpenAI fake JWTs) are minted again so
  that Codex sees the new `exp`. A replaced access surrogate keeps
  standing for the current access token until the old token's expiry
  (1 hour if unknown), at most 4 kept.
- Surrogate entropy: prefix + 48 CSPRNG bytes. Fake JWTs have the
  header `alg: none`, a 32-byte nonce claim and a 32-byte random
  signature.

## Network services: API hosts

- Credential swap only. On `api.anthropic.com` and `chatgpt.com`, all
  paths, only `Authorization: Bearer <token>` and `x-api-key: <value>`
  change. They change only when the whole token or value is a known
  access surrogate (or a still valid previous one) or an API-key
  surrogate of the service. The real header is marked sensitive. No
  other header changes and bodies never change. A swap in all headers
  was rejected because upstreams reflect headers (`Origin` into
  `Access-Control-Allow-Origin`, `anthropic-beta` into error messages),
  and chatgpt.com is a full web app that can store and echo values.
  That would make the swap a token oracle.
- Strict credentials. The value must be such a surrogate or a masked
  secret that airlock injected. An unknown surrogate gets the
  provider's "sign in again". A refresh or ID-token surrogate, or any
  other value, gets 401 `airlock_foreign_credential`, so the real
  refresh token never goes to an API host. The store is read for the
  swap only when a credential is surrogate-shaped. chatgpt.com no longer
  has a "canonical `/backend-api/` paths only" rule: the whole host is
  the API, and the answer scan covers all paths.

## Network services: streaming answer scan (`services/scan.rs`)

The previous backstop read only JSON answers with a `Content-Length` up
to 64 KiB. Chunked, most HTTP/2, large, SSE and non-JSON answers passed
unread. Now every API answer streams through a scanner:

- API requests are sent with `Accept-Encoding: identity`. A compressed
  answer is refused (502), because decompressing to scan would add a
  second parser. A response header with a real token refuses the answer,
  `Set-Cookie` included.
- Primary check, always on: an exact search for the real values the
  proxy knows. These are every real token in the service's store (the
  snapshot is read for every API answer) and the masked secrets
  injected into this request. This also finds opaque tokens of any
  format. Minimum lengths: 8 for store tokens, 16 for injected secrets.
  An unreadable store fails the answer (fail closed) and logs one
  warning that names the token store.
- Second check, not for `text/event-stream`: strict token shapes at the
  start of a run of token characters (`[A-Za-z0-9._~-]`; `=`, `/`, `+`
  end a run, so `name=<token>` and URL paths are found). Shapes:
  `sk-ant-(oat|ort|api)NN-` with at least 80 base64url characters, and
  JWTs with an OpenAI claim or issuer (not airlock's fake JWTs). Event
  streams are model output, which can hold token-shaped examples, so
  they get only the exact search.
- Split tokens: the token run at the end of a chunk is held back (up to
  32 KiB hold, longer runs go on in parts), and so is an end of the
  chunk that starts a known value. Everything before it goes on at
  once, so SSE events are not delayed.
- Bounded work: candidates are at most 8 KiB and are checked in place.
  Offsets that were already decided are not searched again. A test
  sends 1 MB adversarial bodies in chunks from 1 byte to 400 KiB.
- Timing: a JSON answer is held until 64 KiB are scanned, so a hit is a
  clean local 502. Other answers go on at once, status included. A hit
  ends the stream with a body error (HTTP/1.1 connection closed, HTTP/2
  stream reset), and the chunk with the token is never sent. Trailers
  are checked like headers.
- The token-host backstop for allowed routes stays: JSON answers up to
  64 KiB with a token field, a code field or a real token are refused.
  It names the JSON path of the field it refused.

## Network services: one store record per service (`services/store.rs`)

- One database `services` with three keys per service.
  `<service>.secrets` holds all grants as one sealed JSON document
  (ChaCha20-Poly1305, AAD = key name + format version).
  `<service>.meta` is plain JSON for `airlock show` (no tokens or
  surrogates). `<service>.generation` is a big-endian u64.
- A write is one read-modify-write transaction over all three keys.
  A reader checks the generation and decrypts and rebuilds its
  surrogate index only when the generation changed. Lookups see
  sign-outs and refreshes of other processes immediately, without a
  cache TTL. The refresh relay does not need a separate uncached read.
  The files never map a surrogate to a grant in plain text, and there is
  no HMAC lookup table. One document per service is small (a few
  grants).

## Refusal logs

Every local refusal of the services logs a warning with what was
refused and why, never a secret value. This includes `token_error` /
`server_error` reasons, the reason `swap_code` refused a code,
`route_not_allowed`, `foreign_credential`, "sign in again", the API-key
rate limit, the backstop's JSON path, and sign-in callback refusals
(warnings, not debug). An upstream token-endpoint error that is relayed
unchanged logs its status and error code at info level.

## What stays hard-coded

Hosts and token-host routes (the security boundary). Token formats
(fail-closed needs to recognize real tokens). Callback ports and paths,
device and manual redirects (no arbitrary host ports; the code channel
binds an exchange to its sign-in). The refresh request shape and
whether the provider revokes access tokens (they follow the agents' own
requests). `create_api_key` limits (it mints long-lived secrets).

## Known limits and unverified points

- WebSocket frames after `101` are not scanned. An encoded token (JSON
  `\u` escape, base64, URL-encoding) is not found. This is accepted:
  the sandbox can make the proxy send a real token upstream only in the
  swapped credential headers, so it cannot steer an echo into an
  encoding.
- Manual Claude sign-in: see the PKCE limit above. It does not work
  without the browser bridge.
- `mcp-proxy.anthropic.com` is not owned. It is not verified which
  credential Claude Code sends there; a surrogate sent there stays a
  surrogate.
- Verified with the real providers (macOS): the Claude browser
  sign-in, the Codex browser sign-in and `codex login
  --device-auth`. Not verified: refresh and revoke with a stored
  non-default client id, `create_api_key` with the non-strict walk,
  Codex refresh answers that omit fields, and the fake-JWT re-mint on a
  real refresh. The route lists and request shapes come from the Claude
  Code 2.1.283 and codex 0.155.1 binaries.
- Two refreshes of the same grant that race both go upstream, and the
  later write wins, as on a host.
