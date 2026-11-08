# Plan: one agent process per ACP session handle

## Context

`acp/agent/{mod,shared,session}.rs` (~2470 lines) share one agent process per
sandbox between many session handles. The shared agent needs routing tables
(owners, prompt order, members), a link protocol and many state enums
(`HandleState`, `ReopenSlot`, `Join`, `Link`, late/drain/dropped). The reopen
after agent exit (WIP a1ab1f06, 70ef2bb9) made this hard to follow. The
router also guesses the session of new session ids (known gap, 2026-11-04 log).

Decision (user): each session handle, also one-shot handles (session/list,
listed-session delete), gets its own agent process. One async task per handle
owns all state as local variables. State transitions are control flow. The
proxy (`proxy.rs`) and the `SessionHandle` trait do not change.

Trade-offs: N sessions = N agent processes, handshakes and sandbox
connections. A second session in a sandbox waits for its own agent start.
Each `session/list` starts its own short agent.

## Findings

1. No separate locate before launch: `launch()` runs the same find and gives
   `NoSandbox` before it starts anything. `locate()` stays for `guest_cwd`.
2. Agent-message routing goes away (`owners`, `route()`, prompt order). All
   non-response agent messages go to the handle. A one-shot handle drops them.
   The proxy `owns()` still assigns sub-agent session ids.
3. One-shot exit code stays 0: do not record the stop code of a one-shot
   agent. A one-shot agent that exits by itself before its response records
   its code.
4. Requests open at a detach get `agent_exited(sandbox, code)`.
   `SESSION_DETACHED` becomes dead: delete it.
5. Record an agent exit code only after its handshake completed. A stop or
   kill before the handshake and a failed start give 0 (as today).
6. Deleted routing state: `prompts`, `last_prompt`, `next_prompt`, `owners`,
   `route()`, `Join`, `dropped`, `reply_dropped`, `Link`, `Command`.

## Behaviors kept (checklist)

- Inbox is always read. `None` sets `closing` and disables that branch (no
  spin). Stop is in-band and immediate in every gate state, also during an
  MCP registration (registration is dropped, as today).
- While the gate is `Reopen`, a request over 64 waiting gets
  `REOPEN_QUEUE_FULL` (text stays).
- Start-failure `tracing::warn!("agent start failed: ...")` with method and
  params cut at `MAX_LOGGED_PARAMS` (bats `tests/cli/acp.bats` checks it).
- Output stream is fused after `Exit`. Stderr relay runs inside the stream.
  `on_line` drops `Line`/`TooLong` while `limited`.
- `HandleMessage::Ready` once per agent after handshake + first request,
  never for one-shot. `Ended` after MCP release, not at agent loss of a
  claimed session. Close in lost phase gets `{}` at once.
- `inbox.close()` before `Ended`; late requests get `SESSION_AGENT_EXITED`.

## Module layout

| File | Contents | ≈ lines |
|---|---|---|
| `agent/mod.rs` | docs, `AgentConfig`, `factory()`, `Stderr` (Clone), `SandboxAgentsInit`, `Pool` (no `agents`), `SandboxAgents`, `Handle { tx, task }`, `Listed` | 260 |
| `agent/session.rs` (rewrite) | `run_session`, `Session` (state across agents), `Gate`, `AgentEnd`, `Step`, `run_agent`, `on_line`, `forward`/`prepare`, `stop`/`wait_exit`, `lose`, `fail_*`, `finish`, notices, `Reopen` | 620 |
| `agent/process.rs` (new) | `Agent` (one running process: location, input, control, mcp, notices, output stream, init, killed, limited): `send` (try_send, kill on full), `kill`, `kill_for_limit`, `close_input`; `check_handshake`, `reopen_method`, `StartFailure`, `trace`, `hide_mcp_secrets` + test | 260 |
| `agent/output.rs` (new) | `enum Output { Line, TooLong, Exit(i32) }`, `fn output(events, max, stderr, sandbox) -> LocalBoxStream<Output>` with `stream::unfold`: framing, line + partial limits, discard after TooLong, stderr relay with drop count, partial flush at exit, relay close without exit → `Exit(1)`. Unit tests | 180 |
| `agent/shared.rs` | deleted | −1297 |

## Sketch

```rust
struct Handle { tx: mpsc::UnboundedSender<Value>, task: JoinHandle<i32> }
// send: tx.send(msg).is_ok()
// shutdown: drop(tx) (in-band stop after earlier messages); task.await.unwrap_or(1)

enum Gate { Handshake, Open, Registering(mcp::Registration), Reopen }
enum AgentEnd { Stopped, Lost, StartFailed(StartFailure), ReopenFailed(String) }

async fn run_session(pool, hid, cwd, first, mut inbox) -> i32 {
    let mut s = Session::new(..);           // one_shot, first key/method/sid, claimed, reopen, mcp, open, lists, code
    let mut waiting = VecDeque::from([first]);
    let mut reopening = false;
    loop {
        match s.run_agent(&mut inbox, &mut waiting, reopening).await {
            AgentEnd::Stopped => break,
            AgentEnd::StartFailed(f) if reopening => s.fail_reopen(&mut waiting, &f.message).await,
            AgentEnd::StartFailed(f) => { s.fail_start(&mut waiting, &f).await; break }
            AgentEnd::ReopenFailed(d) => s.fail_reopen(&mut waiting, &d).await,
            AgentEnd::Lost if s.reopen.is_none() => break,
            AgentEnd::Lost => {}
        }
        // Lost phase: close gets {} at once, other non-requests dropped.
        let Some(trigger) = s.next_request(&mut inbox).await else { break };
        waiting = VecDeque::from([s.reopen_request(), trigger]);
        reopening = true;
    }
    s.finish(inbox).await  // inbox.close(); late requests -> SESSION_AGENT_EXITED;
                           // timeout(grace, mcp.release_all()); Ended; return code
}
```

`run_agent`: launch in `select!` with the inbox (inbox close drops the launch);
`NoSandbox` on a list → empty list; send `initialize`; then a loop: forward
waiting messages while the gate is open; `select!` over inbox (cap 64),
MCP registration, agent output (lines → `on_line`, TooLong, Exit → `lose`),
handshake deadline, kill token. At the end: `stop` (close input, wait grace,
kill, wait grace), record code (not for one-shot), fail open requests with
`agent_exited`.

`lose` (agent exit by itself): before handshake → `StartFailed(AGENT_EXITED)`;
else record code; stopping → fail open, `Stopped`; reopen pending →
`ReopenFailed`; else fail open + waiting, notice if claimed, release MCP with
grace, `Lost` (no `Ended`).

`on_line`: INIT_ID reply → handshake; response bookkeeping (`mcp.on_response`,
open, lists map, `Listed::record` for one-shot); OK response to first request →
claim + reopen data; reopen-id response hidden → `Reopened`; one-shot drops
non-responses; restrict filter `allowed()`, `add_auth_hint`, hidden replay of
`session/update` for the reopen sid while gate is `Reopen`; claim starts
`show_notices` (spawn_local); one-shot first response → done.

`forward`: reopen request gets method from `reopen_method`, else
`REOPEN_NOT_SUPPORTED`; `Ready` once per agent for non-one-shot; `prepare`
(cwd map, `check_client_request`, `mcp.prepare` → Registering, or
`mcp::remove_all` + `track`); refusal of reopen id → `REOPEN_REFUSED`; refusal
of one-shot first request → done after the error reply.

Backpressure: output is not read only during `messages.send` (intended) and
the inline foreign-cwd locate (short, no sandbox start).

## Tests

- Delete `test_shared_agent.rs` (removed feature).
- `test_multi_sandbox.rs` → `test_session_agents.rs`:
  `each_session_runs_in_own_agent` (launched a, a, b; different pids; prompts
  and permission requests stay in their session),
  `session_close_stops_only_its_agent`, keep
  `agent_exit_ends_its_session_and_keeps_others`.
- `test_session_list.rs`: rustdoc step only.
- `test_sandbox_notices.rs`: `launches == 2`; rustdoc.
- `test_session_dirs.rs`: `launched == ["a","b","a"]`.
- New checks (from the deleted shared tests):
  - list in a sandbox with an open session starts a second agent, and the
    session keeps working;
  - `session_close_stops_only_its_agent` with `AGENT_EOF=5`: `params.last`
    after close, exit code 5, a new session launches a new agent;
  - agent exit of one session keeps a second session of the same sandbox
    working;
  - one-shot list with `AGENT_EOF=5` gives proxy exit code 0;
  - fork starts its own agent (`fork_keeps_source_servers`,
    `test_capabilities` fork).
  - handshake-failure tests assert exit code 0.
- Rustdoc steps to update: `test_session_dirs` step 3,
  `test_sandbox_notices` step 5, `test_mcp_lifecycle` ("each sandbox has its
  own agent"), `test_session_list` step 5, wording in `test_agent_exit`,
  `test_agent_ends`.

## Docs

- `acp/mod.rs`, `agent/*.rs` module docs (no type or function names);
  `launcher.rs` module doc and "share this agent" comment; `messages.rs`
  texts with "shared" wording.
- Manual `docs/manual/src/tips/editor-acp.md` "Many projects" and
  "Session list and delete": one agent per session (STE skill + lint).
- New `docs/log/2026-11-06-acp-agent-per-session.md`: reverses the shared
  agent of the 2026-11-04 multi-sandbox design, closes the "guesses the
  session" gap, exit code = first non-zero by end order.

## Phases (each: `mise format`, `mise run test`, `mise run lint` green; WIP commit; reviewer)

1. New modules only: `agent/output.rs` (stream + unit tests) and
   `agent/process.rs` (`Agent`, helpers). `shared.rs` not touched; temporary
   `#[allow(dead_code)]`.
2. Session task core in a new file next to `shared.rs`: `run_session`,
   `run_agent`, `stop`, `lose`, `finish` for `session/new`, prompts and
   one-shot. Switch `create()` to it; skip reopen and MCP gating tests
   temporarily only if unavoidable (none skipped at the commit).
3. Reopen and MCP gating in the session task.
4. Delete `shared.rs`, old `session.rs` parts and `SESSION_DETACHED`; test
   changes and new tests.
5. Docs, manual, log entry, `/simplify` pass, `handle = hid` log field.

Estimate: agent/ ~2470 → ~1510 lines.

## Risks

- Agents that keep sessions only in memory fail fork/load/delete in a new
  process (Claude, Codex store on disk). Note in log.
- Resources: N processes; parallel writes to agent config files.
- Stop waits for a running MCP registration (≤ `mcp_register_timeout`);
  kill token kills at once.
- `inbox.close()` before `Ended`, else a late request gets no answer.
- Panicking task → exit code 1.
- Load of an open session: the proxy stops the old handle and starts the new
  one at once. The new agent can load while the old agent still saves at
  EOF. Shared mode had one process. Accept and note in the log (Claude saves
  each message at once).
- Two sessions that start a stopped sandbox at the same time: `Sandbox::run`
  locks; check by hand.
- Slower tests (more fake agents).

## Verification

- `mise run test`, `mise run lint` after each phase.
- Manual: `.tmp/test-acp`, `mise airlock -- acp ...` with AIR: two sessions in
  one project, `airlock stop`, prompt → normal reply; session list works;
  two sessions opened at the same time in a stopped sandbox.
