# Remote Control worker tokens

Claude Code `/remote-control` failed in airlock with "Transport recovery
exhausted (code 4094)". `POST /v1/code/sessions/{id}/bridge` gives a
`worker_jwt`. The worker transport then sends it as `Authorization:
Bearer` to `/v1/code/sessions/{id}/worker/...`. The strict credential
swap of the `anthropic` service did not know this token and answered
401 `airlock_foreign_credential`. Claude retried and stopped.

A first fix let any JWT pass on the session routes. That let a guest
bring the worker token of a session in another account and send data to
it through the API host, which is always allowed. Thus the service now
keeps the worker tokens that it sees Anthropic mint:

* `bridge` goes upstream with the normal swap and answer scan. The
  service keeps the `worker_jwt` of a 2xx answer for the session id of
  the path. The request must use the sign-in or an injected secret, the
  `api_base_url` must be the API host, and the token must be a JWT. Else
  the guest gets 502.
* The guest gets a surrogate in place of the real token. The surrogate
  keeps the optional `sk-ant-si-` prefix, the JWT header and the claims,
  because Claude Code reads `exp` and `session_id` from them. Only the
  signature is random. A worker token is valid for hours. If the real
  token leaks out of the sandbox, someone outside can act as the worker
  for that time.
* On `/v1/code/sessions/{id}/worker` and below, a kept surrogate of the
  same session id goes upstream as the real token. The real token itself
  is no credential. Other values keep the strict swap.
* The answer scan of all API answers also searches for the kept real
  worker tokens. This includes the `bridge` answer after the change, so
  a copy of the real token in another field also gets 502.
* The service keeps the last 3 tokens of a session, because a new
  `bridge` call does not stop the streams of the old token at once. It
  keeps at most 8 sessions. A sandbox normally runs one session, and
  each kept real token makes the scan of each API answer slower. Tokens do not expire in airlock, because
  the heartbeat can make a token live longer. The upstream checks
  validity.
* The tokens are only in memory. After a restart, Claude Code calls
  `bridge` again. The server mode (`claude remote-control`) is not
  supported.

The `claude@2` pack set `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`. That
mode also turns off Remote Control. The pack now sets
`DISABLE_TELEMETRY` and `DISABLE_ERROR_REPORTING` in its place.
