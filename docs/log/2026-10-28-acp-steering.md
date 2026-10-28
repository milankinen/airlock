# ACP steering

JetBrains IDEs enable "Send now" (steer a running turn) only when the
`initialize` result has the top-level `_meta.steering.supported: true`.
The proxy answers `initialize` before the agent runs, so it must set the
flag itself.

## Change

- The `initialize` result of the proxy has
  `_meta.steering.supported: true`. A repeated `initialize` gets the same
  result.
- The client request `_session/steering` goes to the agent only when the
  `initialize` result of the agent has the same flag. Otherwise the proxy
  refuses it with `METHOD_NOT_FOUND`.
- The proxy checks `params.prompt` of a steering request like the prompt
  of `session/prompt`. It refuses image, audio and resource content that
  the agent does not support.
- In restrict mode, the proxy still drops the `_meta` of the client capabilities.
