# Ad-hoc port forwards (`airlock network forward`)

Config port forwards (`[network.ports.<name>]`) are fixed at boot. A
dev server that starts later in the sandbox could not be published
without a restart. The new command publishes ports while it runs:

```
airlock network forward [--mode=guest|host] [--follow-git-worktree] <forward>...
```

`<forward>` uses the config syntax (`8080`, `9000:3000`, left = host).
`--mode=guest` (default) matches config `guest = [...]`, `--mode=host`
matches config `host = [...]`.

## Ownership: the forward process owns the listeners

The sandbox process keeps no registry of ad-hoc forwards. Everything
hangs off the forward process's `cli.sock` connection:

```
airlock network forward ──cli.sock (CliService)──▶ airlock start ──vsock (Supervisor)──▶ airlockd
```

- **Guest mode**: the forward process binds the host listeners itself
  (`reverse_forward::bind`, v4 + v6). Each accepted connection calls
  `CliService.connectToGuestPort`, which passes straight to
  `Supervisor.connectToGuestPort`. `GuestNetwork` works over either client,
  so `reverse_forward::serve` is reused.
- **Host mode**: new `Supervisor.bindHostPort(port, connector) ->
  handle`. airlockd binds `127.0.0.1:<port>` and calls
  `connector.connect` for each accepted connection. The forward process
  implements `LocalConnector` and connects to host `127.0.0.1`, then
  `[::1]`. `ListenHandle` is a lifetime token: its server `Drop`
  aborts the accept task (listener plus connection `JoinSet`).

When the forward process exits, the unix socket closes. capnp-rpc then
drops the caps it proxied to the vsock connection, so the guest
`ListenHandle` drops and the port frees. When the sandbox stops,
`stop_services` aborts the cli server, the forward process sees its
`RpcSystem` end and exits with code 1. Ctrl+C exits with 0.

## Checks

- **Fail all at start**: duplicate ports in the args, host bind errors
  (guest mode) and guest bind errors (host mode) fail the command. In
  host mode, earlier handles drop on the first error.
- **Policy**: host-mode connections go through a sandbox-side wrapper
  of the connector. It checks `NetworkControl::allows_port_forward()`
  on each connect (`deny-always` or public-only network deny), the
  same rule as `Network::resolve_target` uses for config host forwards.
  Policy changes at run time apply to new connections.
- **Loops**: the cli server refuses a host-mode listen on a config
  `guest`-list guest port, and a guest-mode open on a config
  `host`-list guest port. Either would relay back to itself.
- Guest mode has no up-front check of the sandbox port, because a
  sandbox program can start to listen later. A failed open shows a
  warning on the first failure and again after a later success.
- Port 0 is refused by the parser and by airlockd.

## Other changes

- `openLocalTcp` became
  `connectToGuestPort` and the new guest listen call is
  `bindHostPort` (same ordinals, in `Supervisor` and
  `CliService`).
- airlockd has one guest listener (`net/guest_listener.rs`) for config
  host ports and ad-hoc host-mode forwards. One bind, accept loop and
  relay. The host side of each connection is either the network proxy
  (config ports, boot-time, live until VM stop) or the `LocalConnector`
  (ad-hoc, abort on handle drop). Both back off 100ms on accept errors.
- `FromStr for PortMapping` shared by clap and the config visitor. The
  config string form still requires a colon.
- The `cli.sock` lookup and connect moved from `exec` into a shared
  sandbox client. It returns the `RpcSystem` unspawned so the caller
  can watch for disconnect.
- `free_port` / echo server test helpers moved to `airlock-test-utils`.

## Limits

Ad-hoc forwards do not show in the network monitor. The guest side
listens on IPv4 `127.0.0.1` only, same as config forwards.
