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

* `bridge` goes upstream with the normal swap and answer scan. If the
  request used the sign-in or an injected secret, the service keeps the
  `worker_jwt` of a 2xx answer for the session id of the path. The
  `api_base_url` must be the API host, else the guest gets 502.
* On `/v1/code/sessions/{id}/worker` and below, a kept token of the same
  session id goes upstream unchanged. The answer scan still applies.
  Other values keep the strict swap.
* The service keeps the last 3 tokens of a session, because a new
  `bridge` call does not stop the streams of the old token at once. It
  keeps at most 64 sessions. Tokens do not expire in airlock, because
  the heartbeat can make a token live longer. The upstream checks
  validity.
* The tokens are only in memory. After a restart, Claude Code calls
  `bridge` again. The server mode (`claude remote-control`) is not
  supported.

The `claude@2` pack set `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`. That
mode also turns off Remote Control. The pack now sets
`DISABLE_TELEMETRY` and `DISABLE_ERROR_REPORTING` in its place.
