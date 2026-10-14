# Plan: `airlock network forward` (ad-hoc port forwards)

## Context

Config port forwards (`[network.ports.<name>]`) are fixed at boot. Users need
to publish ports ad hoc while a sandbox runs, e.g. a dev server started later.
New command:

```
airlock network forward [--mode=guest|host] <forward>...
```

- `--mode=guest` (default): same as config `guest = [...]`. Host process
  connects to `127.0.0.1:<host>` / `[::1]:<host>` and reaches guest
  `127.0.0.1:<guest>`.
- `--mode=host`: same as config `host = [...]`. Guest process connects to
  `127.0.0.1:<guest>` and reaches host `127.0.0.1:<host>`.
- `<forward>`: same syntax as config `PortMapping`: `8080` or `9000:3000`
  (left = host port, right = guest port, in both modes). At least one.
- Forwards are active only while the command runs (foreground). Ctrl+C /
  SIGTERM stops it (exit 0). Sandbox stop ends it (message, exit 1).
- No running sandbox → error (same lookup and messages as `airlock exec`).

User decisions:
- Port conflict (host port in use, guest port in use, port in config
  forward, duplicate in args) → fail all, no forward stays active.
- Listeners are owned by the forward process (host listeners) or by
  capabilities that the forward process holds (guest listeners). Process
  exit or sandbox stop releases everything, with no registry in the
  sandbox process.
- Output: list of active forwards at start, then quiet. No per-connection
  lines.

## High-level design

```
airlock network forward ──cli.sock (CliService)──▶ airlock start ──vsock (Supervisor)──▶ airlockd
```

**Guest mode** (host → guest):
1. Forward process binds host listeners with existing
   `network::reverse_forward::bind` (v4 + v6, fail on first error).
2. Per accepted connection: call new `CliService.connectToGuestPort(port, client)
   -> (server)`. Sandbox process relays it to existing
   `Supervisor.connectToGuestPort`. Bytes relay with existing `tcp::relay` (same as
   `reverse_forward::relay_raw`, with a CLI-side `GuestNetwork`-like
   connector).
3. Config guest ports are already bound by the sandbox process, so the host
   bind fails → conflict detection is free.

**Host mode** (guest → host):
1. New supervisor RPC in airlockd:
   ```capnp
   # Guest → host port forward at run time. The guest binds
   # 127.0.0.1:<port> and calls `connector.connect` for each accepted
   # connection. The listener closes when the host drops `handle`.
   bindHostPort @9 (port :UInt16, connector :LocalConnector) -> (handle :ListenHandle);
   interface LocalConnector { connect @0 (client :Network.TcpSink) -> (server :Network.TcpSink); }
   interface ListenHandle {}   # lifetime token only
   ```
   (Put `LocalConnector` / `ListenHandle` in `supervisor.capnp`; `cli.capnp`
   reuses them.)
2. airlockd (`app/airlockd/src/net/`, new `guest_listener.rs` next to
   `host_port_forward.rs`): bind `127.0.0.1:<port>` (error → capnp failed
   `bind 127.0.0.1:<port>: ...`), spawn accept loop with `spawn_local`.
   `ListenHandle` server impl owns an abort handle; its `Drop` aborts the
   accept loop and its connections. Reuse `rpc_bridge::{ChannelSink, relay}`.
   Refuse before `boot` like `spawn`. Config host ports are already bound by
   airlockd → conflict fails naturally (EADDRINUSE). Guest processes share
   airlockd's network namespace (only mount/ipc/uts are unshared).
3. New `CliService.bindHostPort(port, connector) -> (handle)` relays to the
   supervisor call. The forward process implements `LocalConnector`: on
   `connect`, `TcpStream::connect(127.0.0.1:<host_port>)` and relay. No
   network policy/interception, same as config host forwards (they are
   `port_forwarded` passthrough).
4. Lifetime: when the forward process exits, the unix-socket RPC connection
   drops → sandbox process drops imported/proxied caps → guest `ListenHandle`
   drops → listener closes. When any requested listen fails, the forward
   process drops the handles it already has and exits with error.

**Sandbox stop detection**: forward process `select!`s on the RPC system
future (ends on disconnect), Ctrl+C (`cli::is_interrupted` / signal), and
the accept loops. The sandbox shutdown aborts the cli server
(`stop_services`), which closes the socket.

**Shared client code**: move `find_cli_sock` + connect + bootstrap out of
`cli/cmd_exec.rs` into a shared module (e.g. `cli/sandbox_client.rs`) used by
`exec` and `network forward`. Rename the `exec_*` messages that become
shared (no-running-sandbox, stale sock, connect failed, attached-to) to
neutral names.

**Forward spec parsing**: add `impl FromStr for PortMapping`
(`config/config_values.rs`) and make the serde visitor use it, so CLI and
config share one parser and one error text.

## Review fixes (Plan-agent review, round 1)

1. **Policy (security)**: config host forwards are denied under
   `deny-always` and under `public_only` (`Network::resolve_target`,
   `network/mod.rs:433`). Host mode must match. Pass network access
   (`NetworkControl` / a small `allows_port_forward()` check) into
   `cli_server::serve`. The sandbox process wraps the client's
   `LocalConnector` and checks the policy on **each** `connect` (policy can
   change at run time). Guest mode is not policy-gated (host is trusted),
   same as config `guest` forwards.
2. **Forward loops**: reject forwards that overlap a config forward of the
   opposite direction (e.g. `--mode=host 8000` with config `guest = [8000]`
   → guest 8000 → host 8000 → guest 8000). `cli_server` gets the config
   lists (`rules.rs:171`, `:192`) and refuses the request with a clear error.
3. **Port 0** rejected by the parser.
4. **Parser**: `FromStr for PortMapping` accepts `8080` and `9000:3000`.
   Serde `visit_str` keeps its colon check (config string `"8080"` stays an
   error), then calls `FromStr`.
5. airlockd: non-Linux stub in `net/mod.rs` next to the other stubs; update
   module doc. Boot gate via `spawn_creds` like `spawn` (`rpc.rs:364`).
   Accept loop holds no `Rc` back to the handle.
6. `ForwardArgs` gets `--follow-git-worktree` like `exec`.
7. Client-side guest connector: make `GuestNetwork` (`rpc/guest_network.rs`)
   work over either `supervisor::Client` or `cli_service::Client` (enum or
   small trait), so `reverse_forward::bind` + `serve` are reused unchanged.
8. Bind error texts say "reverse port forward" (`messages.rs:1077`): add
   neutral or command-specific texts.
9. Ad-hoc forwards do not show in the network monitor; guest listens on
   `127.0.0.1` only (same as config). Document both.
10. Keep the RPC system future in the client (not `spawn_local` only) to
    `select!` on disconnect; use `cli::interrupted()` (`cli/mod.rs:269`).
    Update `cli_help.bats` snapshot if present. Test host low-port EACCES
    error text.
11. Caps pass straight through between cli.sock and vsock (capnp-rpc 0.26
    proxies them and releases them on disconnect). `StdinBridge` has no
    documented reason; leave it as is.

## Detailed plan (implementation order)

### Task A — schema + airlockd (guest side)
- `app/airlock-common/schema/supervisor.capnp`: add `bindHostPort @9`,
  `LocalConnector`, `ListenHandle` with STE comments.
- `app/airlockd/src/rpc.rs`: implement `bind_host_port`, gated on boot.
- `app/airlockd/src/net/guest_listener.rs` (+ `mod.rs` export): bind, accept
  loop, per-connection: make server `ChannelSink`, call `connector.connect`,
  `relay`. Handle drop aborts loop (`JoinSet` / `AbortHandle` owned by the
  `ListenHandle` server struct).
- Test (airlockd, inline or `net/tests`): real TCP: listen via the RPC with
  a connector backed by a real local echo server; connect to guest port,
  check bytes; drop handle, check port rebinds. Second listen on same port
  fails.
- Gate: `mise run test` (airlockd), clippy.

### Task B — CLI server relay (sandbox process)
- `app/airlock-common/schema/cli.capnp`: add
  `connectToGuestPort @1 (port, client :Network.TcpSink) -> (server :Network.TcpSink)`
  and `bindHostPort @2 (port, connector :Supervisor.LocalConnector) -> (handle :Supervisor.ListenHandle)`.
  Update interface comment (no longer exec-only).
- `app/airlock-cli/src/rpc/supervisor.rs` (`Supervisor` wrapper): add
  `bind_host_port`. Reuse `GuestNetwork` for connectToGuestPort or pass caps.
- `app/airlock-cli/src/cli_server.rs`: implement both methods; pass the
  received caps straight to the supervisor (capnp proxies across
  connections). Update module doc ("Server for `airlock exec`" →
  sandbox-attach clients).
- Depends on A (schema).

### Task C — `airlock network forward` command (client)
- `config/config_values.rs`: `FromStr for PortMapping`; serde uses it.
- `cli/sandbox_client.rs`: shared lookup/connect from `cmd_exec.rs`;
  `cmd_exec.rs` uses it. Messages renamed in `messages.rs` `cli` module.
- `cli/cmd_network.rs`: `NetworkArgs { #[command(subcommand)] Forward(ForwardArgs) }`,
  `ForwardArgs { #[arg(long, value_enum, default_value_t = Mode::Guest)] mode, #[arg(required = true)] forwards: Vec<PortMapping> }`.
  Register in `cli/mod.rs` and `main.rs` `Command` enum + match.
- Flow:
  1. Check duplicates in args (guest mode: host port; host mode: guest
     port) → error.
  2. Find + connect cli.sock (errors as exec).
  3. Guest mode: `reverse_forward::bind(...)` all, then accept loops
     (`serve_with` with a handler calling `CliService.connectToGuestPort`).
     Host mode: call `bindHostPort` for each forward; keep handles; on
     first error drop all and fail.
  4. Print active forwards (messages in `messages.rs`, new
     `pub mod network_forward` or into `network`), e.g.
     `Forwarding (Ctrl+C to stop):` + `  localhost:8080 → sandbox:80`.
  5. `select!` on RPC disconnect (message "sandbox stopped", exit 1) and
     interrupt (exit 0).
- Test `app/airlock-cli/src/cli/tests/test_network_forward.rs` (e2e, real
  TCP): run real `cli_server::serve` on a temp socket with a fake
  supervisor (edge) whose `connectToGuestPort`/`bindHostPort` use real local
  TCP; run the forward logic; check bytes in both modes; check port
  conflict fails all; check server abort ends the forward command. If the
  current `Supervisor` wrapper is hard to fake, fake at the capnp
  `supervisor::Server` level via `test_cfg::rpc_loopback`.
- bats `tests/cli/network.bats`: "network forward without running sandbox
  fails"; bad forward spec fails.
- Depends on A, B.

### Task D — docs
- Manual: `docs/manual/src/usage/attaching-to-running-sandbox.md` (or a new
  usage page in `SUMMARY.md`) describing the command; link from
  `configuration/network.md` "Port forwarding". Use `/asd-ste100`, run the
  manual linter.
- Dev log `docs/log/2026-10-14-ad-hoc-port-forwards.md`.

## Verification
- `mise run test`, `mise run bats`, `mise format`, clippy (check mise tasks).
- Manual: `.tmp/test-forward`, `mise airlock -- start -- python3 -m http.server 8000`;
  other terminal `mise airlock -- network forward 8000` → `curl localhost:8000`.
  Host mode: run host `python3 -m http.server 9000`, `network forward --mode=host 9000`,
  then `mise airlock -- exec -- curl localhost:9000`. Ctrl+C → port freed
  on both sides. Stop sandbox → forward exits. Second forward on same port
  → error.

## Process
- Producers: A, B (opus), C (opus), D (sonnet). Critic per task with a
  different tier. WIP commit after each working task.
