//! Helpers for the ACP proxy tests: a fake agent that runs as a local
//! process, fake sandboxes, a fake sandbox that adds the editor MCP
//! servers, and a client that talks to the proxy.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::cli_capnp::cli_service;
use futures::future::LocalBoxFuture;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::acp::{
    AGENT_EVENTS, AGENT_INPUT_LINES, AgentProcess, AgentProcessLauncher, Claim, Config, Control,
    HandleId, HandleMessage, Location, SessionContext, SessionFactory, SessionFactoryInit,
    SessionHandle,
};
use crate::rpc::ProcessEvent;
use crate::runtime::SignalStream;
use crate::sandbox::{NoticeLog, NoticeReader};
use crate::test_cfg::mcp_grant::{FakeGrant, GrantEvent, Route};

/// Fake ACP agent. It writes the lines of `$AGENT_PRE`, replies to the
/// proxy handshake with `$AGENT_INIT` and then writes the lines of
/// `$AGENT_SAY`. For each other input line, it writes a `session/update`
/// notification with the line in `params.seen` and its sandbox in
/// `params.from`. It replies to each request with a numeric id with an
/// empty result, or with the error `$AGENT_ERROR`. Other settings:
///  - `AGENT_SANDBOX`: Sandbox of the agent. The launcher sets it.
///  - `AGENT_NO_INIT`: Never reply to the handshake
///  - `AGENT_INIT_DELAY`: Seconds to wait before the handshake reply
///  - `AGENT_STDERR`: Number of bytes to write to stderr before the
///    handshake reply
///  - `AGENT_FLOOD`: After the handshake, write one line without end and
///    read no more input
///  - `AGENT_EOF`: Exit code at the end of the input, or `ignore` to wait
///    for the kill
///  - `AGENT_SESSION_IDS`: Reply to `session/new` and `session/fork` with
///    the result `{"sessionId":"<prefix><N>"}`, where N counts from 1.
///    If not set, the session id is `<sandbox>-<pid>-<N>`.
///  - `AGENT_SESSION_PREFIX`: The prefix of the session ids, `s` if not set
///  - `AGENT_LIST`: Reply to `session/list` with this result
///  - `AGENT_ASK`: For each `session/prompt`, send a
///    `session/request_permission` with this JSON id and the session of the
///    prompt before the reply
///  - `AGENT_HOLD`: Requests with this method get no reply, `test/hold` if
///    not set. For each one, the agent writes the lines of `$AGENT_ON_HOLD`.
///  - `AGENT_DIE`: Exit after this number of seconds from the start
///  - `AGENT_TRAP`: At `SIGUSR1`, wait 200 ms and exit with this code
///
/// A request with the param `"cursor":"bad"` gets an error.
///
/// The message `test/exit` makes it write a `session/update` notification
/// with `params.last` and no newline, and exit with the code
/// `$AGENT_EXIT_CODE`, 7 if not set. A `test/exit` request gets no reply.
/// At the end of its input, the agent writes the same notification.
pub const FAKE_AGENT: &str = r#"
last() { printf '{"jsonrpc":"2.0","method":"session/update","params":{"last":true}}'; }
seen() { printf '{"jsonrpc":"2.0","method":"session/update","params":{"seen":%s,"from":"%s"}}\n' "$1" "$AGENT_SANDBOX"; }
hold=${AGENT_HOLD:-test/hold}
[ -n "$AGENT_TRAP" ] && trap 'sleep 0.2; exit "$AGENT_TRAP"' USR1
[ -n "$AGENT_DIE" ] && (sleep "$AGENT_DIE"; kill $$) >/dev/null 2>&1 &
while IFS= read -r line; do
  case "$line" in
  *'"id":"airlock-acp-initialize"'*)
    [ -n "$AGENT_PRE" ] && printf '%s\n' "$AGENT_PRE"
    [ -n "$AGENT_NO_INIT" ] && continue
    [ -n "$AGENT_INIT_DELAY" ] && sleep "$AGENT_INIT_DELAY"
    [ -n "$AGENT_STDERR" ] && head -c "$AGENT_STDERR" /dev/zero >&2
    printf '{"jsonrpc":"2.0","id":"airlock-acp-initialize","result":%s}\n' "$AGENT_INIT"
    [ -n "$AGENT_SAY" ] && printf '%s\n' "$AGENT_SAY"
    if [ -n "$AGENT_FLOOD" ]; then
      while :; do printf %s xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done
    fi
    seen "$line" ;;
  *'"method":"test/exit"'*) last; exit "${AGENT_EXIT_CODE:-7}" ;;
  *"\"method\":\"$hold\""*)
    seen "$line"
    [ -n "$AGENT_ON_HOLD" ] && printf '%s\n' "$AGENT_ON_HOLD" ;;
  *)
    seen "$line"
    case "$line" in
    *'"method"'*'"id":'[0-9]*|*'"id":'[0-9]*'"method"'*)
      id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      case "$line" in
      *'"method":"session/prompt"'*)
        if [ -n "$AGENT_ASK" ]; then
          sid=$(printf '%s\n' "$line" | sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p')
          printf '{"jsonrpc":"2.0","id":%s,"method":"session/request_permission","params":{"sessionId":"%s","options":[],"toolCall":{}}}\n' "$AGENT_ASK" "$sid"
        fi ;;
      esac
      if [ -n "$AGENT_ERROR" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":%s}\n' "$id" "$AGENT_ERROR"
      else
        result='{}'
        case "$line" in
        *'"cursor":"bad"'*)
          printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"bad cursor"}}\n' "$id"
          continue ;;
        *'"method":"session/list"'*) [ -n "$AGENT_LIST" ] && result="$AGENT_LIST" ;;
        esac
        case "$line" in
        *'"method":"session/new"'*|*'"method":"session/fork"'*)
          n=$((${n:-0} + 1))
          sid="${AGENT_SANDBOX}-$$-$n"
          [ -n "$AGENT_SESSION_IDS" ] && sid="${AGENT_SESSION_PREFIX:-s}$n"
          result="{\"sessionId\":\"$sid\"}" ;;
        esac
        printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
      fi ;;
    esac ;;
  esac
done
last
[ "$AGENT_EOF" = ignore ] && exec sleep 1000 </dev/null >/dev/null 2>&1
exit "${AGENT_EOF:-0}"
"#;

/// `initialize` result of the fake agent with all features of the proxy.
pub fn full_agent_init() -> Value {
    json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": { "image": true, "embeddedContext": true },
            "mcpCapabilities": { "http": true },
            "sessionCapabilities": {
                "list": {}, "resume": {}, "close": {}, "delete": {}, "fork": {},
            },
        },
    })
}

/// Fake running sandbox of [`LocalProcessLauncher`].
struct FakeSandbox {
    /// Name of the sandbox.
    name: String,
    /// Project dir on the host.
    host_root: String,
    /// Project dir in the guest.
    guest_root: String,
    /// Fake sandbox that adds the editor MCP servers.
    registrar: Option<crate::sandbox::McpRegistrar>,
}

/// Agent process launcher for tests. Runs the agent as a local process. Has two
/// fake running sandboxes: `a` for `/projects/a` and `b` for
/// `/projects/b`, both with the guest project dir `/work`.
pub struct LocalProcessLauncher {
    /// Environment variables of the agent.
    env: Vec<(String, String)>,
    /// Number of agent launches.
    pub launches: Rc<Cell<usize>>,
    /// Sandbox of each agent launch, in order.
    pub launched: Rc<RefCell<Vec<String>>>,
    /// The fake sandboxes.
    sandboxes: Vec<FakeSandbox>,
    /// Notices of the sandbox start for each launch.
    notices: Option<NoticeReader>,
}

impl LocalProcessLauncher {
    /// Make a launcher that gives the agent the environment `env`. The
    /// agent gets the [`full_agent_init`] if `env` has no `AGENT_INIT`.
    pub fn new(env: &[(&str, &str)]) -> Self {
        let mut env = pairs(env);
        if !env.iter().any(|(k, _)| k == "AGENT_INIT") {
            env.push(("AGENT_INIT".into(), full_agent_init().to_string()));
        }
        Self {
            env,
            launches: Rc::default(),
            launched: Rc::default(),
            sandboxes: ["a", "b"]
                .iter()
                .map(|s| FakeSandbox {
                    name: (*s).into(),
                    host_root: format!("/projects/{s}"),
                    guest_root: "/work".into(),
                    registrar: None,
                })
                .collect(),
            notices: None,
        }
    }

    /// Give the agents a fake sandbox with the notices `log`, in all
    /// sandboxes. The test can add notices to `log` later.
    pub fn with_notices(self, log: &NoticeLog) -> Self {
        self.with_fake_notices(log, None)
    }

    /// Give the agents a fake sandbox with the notices `log` (see
    /// [`Self::with_notices`]). The first notice read waits for a gate.
    /// Returns:
    ///   The launcher, and the sender that opens the gate.
    pub fn with_held_notices(self, log: &NoticeLog) -> (Self, oneshot::Sender<()>) {
        let (open, gate) = oneshot::channel();
        (self.with_fake_notices(log, Some(gate)), open)
    }

    /// Give the agents the fake notice sandbox with `log` and `gate`.
    fn with_fake_notices(mut self, log: &NoticeLog, gate: Option<oneshot::Receiver<()>>) -> Self {
        let service = capnp_rpc::new_client(FakeNotices {
            log: log.clone(),
            gate: RefCell::new(gate),
        });
        self.notices = Some(NoticeReader::from_client(service));
        self
    }

    /// Use the host dir `root` as the project dir of sandbox `a`.
    pub fn with_root(self, root: &str) -> Self {
        self.with_sandbox_root("a", root)
    }

    /// Use the host dir `root` as the project dir of `sandbox`.
    pub fn with_sandbox_root(mut self, sandbox: &str, root: &str) -> Self {
        self.sandbox_mut(sandbox).host_root = root.to_string();
        self
    }

    /// Give the agents the fake sandbox `registrar` for the editor MCP
    /// servers, in all sandboxes.
    pub fn with_registrar(mut self, registrar: cli_service::Client) -> Self {
        let registrar = crate::sandbox::McpRegistrar::from_client(registrar);
        let (last, others) = self.sandboxes.split_last_mut().expect("sandboxes");
        for sandbox in others {
            sandbox.registrar = Some(registrar.clone());
        }
        last.registrar = Some(registrar);
        self
    }

    /// Get the fake sandbox `name`. Panics if it does not exist.
    fn sandbox_mut(&mut self, name: &str) -> &mut FakeSandbox {
        self.sandboxes
            .iter_mut()
            .find(|s| s.name == name)
            .expect("known sandbox")
    }

    /// Find the fake sandbox of the dir `cwd`.
    fn find(&self, cwd: &str) -> anyhow::Result<(&FakeSandbox, Location)> {
        for sandbox in &self.sandboxes {
            let Some(rest) = cwd.strip_prefix(sandbox.host_root.as_str()) else {
                continue;
            };
            if rest.is_empty() || rest.starts_with('/') {
                let guest = format!("{}{rest}", sandbox.guest_root.trim_end_matches('/'));
                let location = Location {
                    sandbox: sandbox.name.clone(),
                    guest_cwd: if guest.is_empty() { "/".into() } else { guest },
                    host_root: sandbox.host_root.clone(),
                    guest_root: sandbox.guest_root.clone(),
                };
                return Ok((sandbox, location));
            }
        }
        anyhow::bail!(crate::messages::cli::never_started(cwd))
    }
}

/// Make owned environment pairs.
fn pairs(env: &[(&str, &str)]) -> Vec<(String, String)> {
    env.iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

impl AgentProcessLauncher for LocalProcessLauncher {
    async fn locate(&self, cwd: &str) -> anyhow::Result<Location> {
        Ok(self.find(cwd)?.1)
    }

    async fn launch(&self, cwd: &str, argv: &[String]) -> anyhow::Result<(Location, AgentProcess)> {
        let (sandbox, location) = self.find(cwd)?;
        self.launches.set(self.launches.get() + 1);
        self.launched.borrow_mut().push(sandbox.name.clone());
        let mut env = self.env.clone();
        env.push(("AGENT_SANDBOX".into(), sandbox.name.clone()));
        let mut agent = spawn_agent(argv, &env)?;
        agent.mcp = sandbox.registrar.clone();
        agent.notices = self.notices.clone();
        Ok((location, agent))
    }
}

/// Start `argv` as a local process with the extra environment `env`.
fn spawn_agent(argv: &[String], env: &[(String, String)]) -> anyhow::Result<AgentProcess> {
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .envs(env.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id().expect("running child") as i32;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();

    let (input, mut input_rx) = mpsc::channel::<Vec<u8>>(AGENT_INPUT_LINES);
    tokio::task::spawn_local(async move {
        while let Some(data) = input_rx.recv().await {
            if stdin.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    let (events_tx, events) = mpsc::channel(AGENT_EVENTS);
    tokio::task::spawn_local(async move {
        let (mut out_buf, mut err_buf) = ([0u8; 4096], [0u8; 4096]);
        let (mut out_open, mut err_open) = (true, true);
        // The exit event comes after all output, as from the sandbox.
        while out_open || err_open {
            tokio::select! {
                n = stdout.read(&mut out_buf), if out_open => match n {
                    Ok(n) if n > 0 => {
                        let _ = events_tx.send(ProcessEvent::Stdout(out_buf[..n].to_vec())).await;
                    }
                    _ => out_open = false,
                },
                n = stderr.read(&mut err_buf), if err_open => match n {
                    Ok(n) if n > 0 => {
                        let _ = events_tx.send(ProcessEvent::Stderr(err_buf[..n].to_vec())).await;
                    }
                    _ => err_open = false,
                },
            }
        }
        let status = child.wait().await.unwrap();
        let code = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
        let _ = events_tx.send(ProcessEvent::Exit(code)).await;
    });

    let (control, mut control_rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
        while let Some(request) = control_rx.recv().await {
            let Control::Kill = request;
            // SAFETY: sends a signal to the child process only.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    });

    Ok(AgentProcess {
        input,
        events,
        control,
        mcp: None,
        notices: None,
    })
}

/// Session factory for the proxy routing tests. Its session handles run in
/// memory, and the test plays their agents.
struct FakeSessionsInit {
    created: mpsc::UnboundedSender<FakeSession>,
}

impl SessionFactoryInit for FakeSessionsInit {
    fn init(self: Box<Self>, context: SessionContext) -> Rc<dyn SessionFactory> {
        Rc::new(FakeSessions {
            created: self.created,
            messages: context.messages,
            next_id: Cell::new(0),
            session_dirs: Rc::default(),
        })
    }
}

/// Factory that [`FakeSessionsInit`] makes.
struct FakeSessions {
    created: mpsc::UnboundedSender<FakeSession>,
    messages: mpsc::Sender<(HandleId, HandleMessage)>,
    next_id: Cell<HandleId>,
    /// Host session dir of each listed session.
    session_dirs: Rc<RefCell<HashMap<String, String>>>,
}

impl SessionFactory for FakeSessions {
    fn create(&self, cwd: String, first: Value) -> (HandleId, Box<dyn SessionHandle>) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let cancel = CancellationToken::new();
        let (inbox_tx, inbox) = mpsc::unbounded_channel();
        let (exit_tx, exit) = oneshot::channel();
        let stop_code = Rc::new(Cell::new(0));
        let linger = Rc::new(Cell::new(false));
        let _ = self.created.send(FakeSession {
            id,
            cwd,
            first,
            inbox,
            messages: self.messages.clone(),
            exit: Some(exit_tx),
            stop_code: stop_code.clone(),
            linger: linger.clone(),
            cancel: cancel.clone(),
            session_dirs: self.session_dirs.clone(),
        });
        let proxy = FakeHandle {
            inbox: inbox_tx,
            exit,
            cancel,
            stop_code,
            linger,
        };
        (id, Box::new(proxy))
    }

    fn session_dir(&self, sid: &str) -> Option<String> {
        self.session_dirs.borrow().get(sid).cloned()
    }
}

/// Proxy side of a [`FakeSession`].
struct FakeHandle {
    inbox: mpsc::UnboundedSender<Value>,
    exit: oneshot::Receiver<i32>,
    cancel: CancellationToken,
    stop_code: Rc<Cell<i32>>,
    linger: Rc<Cell<bool>>,
}

impl SessionHandle for FakeHandle {
    fn send(&self, msg: Value) -> bool {
        self.inbox.send(msg).is_ok()
    }

    fn shutdown(self: Box<Self>) -> LocalBoxFuture<'static, i32> {
        let FakeHandle {
            mut exit,
            cancel,
            stop_code,
            linger,
            ..
        } = *self;
        cancel.cancel();
        Box::pin(async move {
            // An agent that exited already gives its own exit code.
            tokio::select! {
                biased;
                code = &mut exit => return code.unwrap_or(0),
                () = cancel.cancelled() => {}
            }
            if linger.get() {
                return exit.await.unwrap_or(0);
            }
            stop_code.get()
        })
    }
}

/// Test side of an in-memory fake session handle. The test plays its agent.
/// When the proxy stops the handle, it ends with its stop code, or at
/// [`FakeSession::exit`] if it lingers.
pub struct FakeSession {
    /// Identity of the handle in the proxy.
    pub id: HandleId,
    /// Host session dir of the session request.
    pub cwd: String,
    /// The session request that started the handle.
    pub first: Value,
    inbox: mpsc::UnboundedReceiver<Value>,
    messages: mpsc::Sender<(HandleId, HandleMessage)>,
    exit: Option<oneshot::Sender<i32>>,
    stop_code: Rc<Cell<i32>>,
    linger: Rc<Cell<bool>>,
    cancel: CancellationToken,
    session_dirs: Rc<RefCell<HashMap<String, String>>>,
}

impl FakeSession {
    /// Make the factory give the host session dir `cwd` for the session
    /// `sid`, or no dir if `cwd` is `None`. The factory does this for a
    /// list or a delete.
    pub fn set_session_dir(&self, sid: &str, cwd: Option<&str>) {
        let mut dirs = self.session_dirs.borrow_mut();
        match cwd {
            Some(cwd) => dirs.insert(sid.to_string(), cwd.to_string()),
            None => dirs.remove(sid),
        };
    }

    /// Wait until the proxy stops the handle. Panics after 10 seconds.
    pub async fn stopped(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.cancel.cancelled())
            .await
            .expect("agent stop in time");
    }

    /// Do not end when the proxy stops the handle. End only at
    /// [`FakeSession::exit`].
    pub fn set_linger(&self) {
        self.linger.set(true);
    }

    /// Send the message `msg` to the client.
    pub async fn say(&self, msg: Value) {
        self.send(HandleMessage::Client { msg, claim: None }).await;
    }

    /// Complete the handshake and reply to the session request with the
    /// session `sid`.
    pub async fn open_session(&self, sid: &str) {
        self.send(HandleMessage::Ready).await;
        let req_id = &self.first["id"];
        let msg = crate::acp::rpc::result_response(req_id, &json!({ "sessionId": sid }));
        let claim = Claim {
            session: sid.to_string(),
            replace: false,
        };
        let claim = Some(claim);
        self.send(HandleMessage::Client { msg, claim }).await;
    }

    /// The next client message for the handle. Panics after 10 seconds.
    pub async fn recv(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), self.inbox.recv())
            .await
            .expect("agent message in time")
            .expect("open agent inbox")
    }

    /// Use `code` as the exit code when the proxy stops the handle.
    pub fn set_stop_code(&self, code: i32) {
        self.stop_code.set(code);
    }

    /// End with the exit code `code`. The handle takes no more messages.
    pub async fn exit(&mut self, code: i32) {
        self.inbox.close();
        if let Some(exit) = self.exit.take() {
            let _ = exit.send(code);
        }
        self.send(HandleMessage::Ended).await;
    }

    // The proxy can have stopped its handle side.
    async fn send(&self, message: HandleMessage) {
        let _ = self.messages.send((self.id, message)).await;
    }
}

/// Fake sandbox CLI service with only `addMcpServer`. It records the
/// routes and reports the release and drop of each grant. The other
/// methods fail.
pub struct FakeRegistrar {
    routes: mpsc::UnboundedSender<Route>,
    events: mpsc::UnboundedSender<GrantEvent>,
    /// Server names that never get a reply.
    stuck: Vec<String>,
}

/// Test side of a [`FakeRegistrar`].
pub struct Registrations {
    routes: mpsc::UnboundedReceiver<Route>,
    events: mpsc::UnboundedReceiver<GrantEvent>,
}

impl FakeRegistrar {
    /// Make the fake service. The servers in `stuck` never get a reply.
    /// Returns:
    ///   The service for [`LocalProcessLauncher::with_registrar`], and its test
    ///   side.
    pub fn start(stuck: &[&str]) -> (cli_service::Client, Registrations) {
        let (routes, routes_rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        let stuck = stuck.iter().map(|s| (*s).to_string()).collect();
        let client = capnp_rpc::new_client(Self {
            routes,
            events,
            stuck,
        });
        let side = Registrations {
            routes: routes_rx,
            events: events_rx,
        };
        (client, side)
    }
}

impl cli_service::Server for FakeRegistrar {
    async fn add_mcp_server(
        self: Rc<Self>,
        params: cli_service::AddMcpServerParams,
        mut results: cli_service::AddMcpServerResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let name = params.get_name()?.to_str()?.to_string();
        if self.stuck.contains(&name) {
            std::future::pending::<()>().await;
        }
        let _ = self.routes.send(Route {
            session: params.get_session()?.to_str()?.to_string(),
            name,
            host: params.get_host()?,
        });
        results
            .get()
            .set_grant(capnp_rpc::new_client(FakeGrant::new(self.events.clone())));
        Ok(())
    }
}

/// Fake sandbox that gives its notice log, all notices since the boot.
// The fake CLI server of a full sandbox needs a network and a guest. The
// proxy uses only the notice log. Thus this fake serves only that.
struct FakeNotices {
    log: NoticeLog,
    /// The first read waits until this gate opens or drops.
    gate: RefCell<Option<oneshot::Receiver<()>>>,
}

impl cli_service::Server for FakeNotices {
    async fn notices(
        self: Rc<Self>,
        _params: cli_service::NoticesParams,
        mut results: cli_service::NoticesResults,
    ) -> Result<(), capnp::Error> {
        let gate = self.gate.borrow_mut().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        let log = self.log.all();
        let mut list = results.get().init_notices(log.len() as u32);
        for (i, item) in log.iter().enumerate() {
            let mut notice = list.reborrow().get(i as u32);
            notice.set_seqno(item.seqno);
            notice.set_text(&item.text);
        }
        Ok(())
    }
}

impl Registrations {
    /// The next route. Panics after 10 seconds without one.
    pub async fn route(&mut self) -> Route {
        tokio::time::timeout(Duration::from_secs(10), self.routes.recv())
            .await
            .expect("route in time")
            .expect("open registrar")
    }

    /// The routes that came so far.
    pub fn routes_now(&mut self) -> Vec<Route> {
        std::iter::from_fn(|| self.routes.try_recv().ok()).collect()
    }

    /// Wait for `count` grant releases. Panics after 10 seconds.
    pub async fn released(&mut self, count: usize) {
        let mut seen = 0;
        while seen < count {
            let event = tokio::time::timeout(Duration::from_secs(10), self.events.recv())
                .await
                .expect("grant release in time")
                .expect("open registrar");
            if event == GrantEvent::Released {
                seen += 1;
            }
        }
    }
}

/// Proxy settings for the fake agent, with short time limits.
/// Args:
///  - `restrict`: Remove the client features that act on the host
pub fn test_config(restrict: bool) -> Config {
    let argv = vec!["sh".into(), "-c".into(), FAKE_AGENT.into()];
    Config {
        agent_capabilities: crate::acp::capabilities::preset(&argv),
        argv,
        name: "fake-agent".into(),
        title: "fake-agent (airlock)".into(),
        version: "test".into(),
        restrict,
        proxy_mcp_servers: false,
        mcp_register_timeout: Duration::from_secs(10),
        handshake_timeout: Duration::from_secs(10),
        shutdown_grace: Duration::from_millis(500),
        signals: Some(test_signals),
        max_line_bytes: 64 * 1024,
    }
}

/// [`test_config`] with the editor MCP bridge on.
pub fn mcp_config(restrict: bool) -> Config {
    Config {
        proxy_mcp_servers: true,
        ..test_config(restrict)
    }
}

/// Key and server name of an MCP address that the agent got.
/// Returns:
///   The key and the name, or `None` if `url` is not an editor MCP
///   address.
pub fn mcp_route(url: &str) -> Option<(String, String)> {
    let path = url.strip_prefix("http://acp.mcp.airlock/")?;
    let (key, name) = path.split_once('/')?;
    Some((key.to_string(), name.to_string()))
}

thread_local! {
    /// Signals for the proxy of the test that runs on this thread.
    static SIGNALS: RefCell<Option<mpsc::UnboundedReceiver<i32>>> = const { RefCell::new(None) };
}

/// Make a sender of fake host signals for the proxy of this test thread.
/// Call it before the proxy watches the signals.
pub fn signal_sender() -> mpsc::UnboundedSender<i32> {
    let (tx, rx) = mpsc::unbounded_channel();
    SIGNALS.with(|s| *s.borrow_mut() = Some(rx));
    tx
}

/// Signal source of [`test_config`]. Gives the signals of
/// [`signal_sender`], or no signals if the test made no sender. The test
/// thus sends no real signal to the test process.
// The signal source type of the proxy needs the `Result`.
#[allow(clippy::unnecessary_wraps)]
fn test_signals() -> anyhow::Result<SignalStream> {
    let Some(rx) = SIGNALS.with(|s| s.borrow_mut().take()) else {
        return Ok(Box::pin(futures::stream::pending()));
    };
    Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|signum| (signum, rx))
    })))
}

/// Editor side of a proxy that runs in a task.
pub struct TestClient {
    input: Option<DuplexStream>,
    output: Lines<BufReader<DuplexStream>>,
    /// Stderr of the proxy. The tests never read it, as an editor that
    /// ignores the stderr.
    _stderr: DuplexStream,
    proxy: JoinHandle<i32>,
    /// Session id of the session that [`TestClient::session`] started.
    pub sid: String,
}

impl TestClient {
    /// Run the proxy with `launcher` and `config` in a task. Call inside a
    /// `LocalSet`.
    pub fn start(launcher: impl AgentProcessLauncher + 'static, config: Config) -> Self {
        let (input, proxy_input) = tokio::io::duplex(1 << 20);
        let (proxy_output, output) = tokio::io::duplex(1 << 20);
        let (proxy_stderr, stderr) = tokio::io::duplex(64 * 1024);
        let proxy = tokio::task::spawn_local(async move {
            let input = BufReader::new(proxy_input);
            crate::acp::serve(launcher, config, input, proxy_output, proxy_stderr).await
        });
        Self {
            input: Some(input),
            output: BufReader::new(output).lines(),
            _stderr: stderr,
            proxy,
            sid: String::new(),
        }
    }

    /// Run the proxy core with in-memory fake session handles in a task. Call
    /// inside a `LocalSet`.
    /// Returns:
    ///   The client, and the receiver of the fake handles in their start
    ///   order.
    pub fn start_fake(config: Config) -> (Self, mpsc::UnboundedReceiver<FakeSession>) {
        let (created, agents) = mpsc::unbounded_channel();
        let (input, proxy_input) = tokio::io::duplex(1 << 20);
        let (proxy_output, output) = tokio::io::duplex(1 << 20);
        let (_, stderr) = tokio::io::duplex(1);
        let proxy = tokio::task::spawn_local(async move {
            let factory = FakeSessionsInit { created };
            let input = BufReader::new(proxy_input);
            crate::acp::route(Box::new(factory), &config.proxy(), input, proxy_output).await
        });
        let client = Self {
            input: Some(input),
            output: BufReader::new(output).lines(),
            _stderr: stderr,
            proxy,
            sid: String::new(),
        };
        (client, agents)
    }

    /// Send the bytes `data` to the proxy.
    pub async fn send_raw(&mut self, data: &[u8]) {
        let input = self.input.as_mut().expect("open input");
        input.write_all(data).await.unwrap();
    }

    /// Send the message `msg` to the proxy as one line.
    pub async fn send(&mut self, msg: &Value) {
        self.send_raw(format!("{msg}\n").as_bytes()).await;
    }

    /// The next message from the proxy. Panics after 10 seconds without
    /// one, or at the end of the output.
    pub async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.output.next_line())
            .await
            .expect("message in time")
            .unwrap()
            .expect("open output");
        serde_json::from_str(&line).unwrap()
    }

    /// Send the notification `method` with `params` to the proxy.
    pub async fn notify(&mut self, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.send(&msg).await;
    }

    /// Send the request `method` with the id `req_id` and `params` to the
    /// proxy, and do not wait for the response.
    pub async fn send_request(&mut self, req_id: i64, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "id": req_id, "method": method, "params": params });
        self.send(&msg).await;
    }

    /// Send a prompt with the id `req_id` to `sid` and wait for its
    /// response.
    /// Returns:
    ///   The response, and the other messages that came before it.
    pub async fn prompt(&mut self, req_id: i64, sid: &str) -> (Value, Vec<Value>) {
        let params = json!({ "sessionId": sid, "prompt": [] });
        self.request(req_id, "session/prompt", params).await
    }

    /// Send a prompt with the id `req_id` to `sid`, and do not wait for
    /// the response.
    pub async fn send_prompt(&mut self, req_id: i64, sid: &str) {
        let params = json!({ "sessionId": sid, "prompt": [] });
        self.send_request(req_id, "session/prompt", params).await;
    }

    /// Send a request and wait for its response.
    /// Returns:
    ///   The response, and the other messages that came before it.
    pub async fn request(
        &mut self,
        req_id: i64,
        method: &str,
        params: Value,
    ) -> (Value, Vec<Value>) {
        self.send_request(req_id, method, params).await;
        self.response(req_id).await
    }

    /// Wait for the response to the request `req_id`.
    /// Returns:
    ///   The response, and the other messages that came before it.
    pub async fn response(&mut self, req_id: i64) -> (Value, Vec<Value>) {
        let mut others = Vec::new();
        loop {
            let msg = self.recv().await;
            if msg.get("method").is_none() && msg["id"] == json!(req_id) {
                return (msg, others);
            }
            others.push(msg);
        }
    }

    /// Send the message `msg` with the session id of
    /// [`TestClient::session`] in its params.
    pub async fn send_in_session(&mut self, mut msg: Value) {
        msg["params"]["sessionId"] = json!(self.sid);
        self.send(&msg).await;
    }

    /// Do the client handshake with the client features `caps`.
    /// Returns:
    ///   The `initialize` result of the proxy.
    pub async fn initialize(&mut self, caps: Value) -> Value {
        let params = json!({ "protocolVersion": 1, "clientCapabilities": caps });
        let (response, _) = self.request(0, "initialize", params).await;
        response["result"].clone()
    }

    /// Start the proxy with `launcher` and `config`, do the client handshake
    /// and start a session in `cwd`. Panics if the session fails. The
    /// session id goes to [`TestClient::sid`].
    pub async fn session(launcher: LocalProcessLauncher, config: Config, cwd: &str) -> Self {
        let mut client = Self::start(launcher, config);
        client.initialize(json!({})).await;
        client.sid = client.new_session(2, cwd).await;
        client
    }

    /// Start a session with the request id `req_id` in `cwd`. Panics if the
    /// session fails or has no session id.
    /// Returns:
    ///   The session id.
    pub async fn new_session(&mut self, req_id: i64, cwd: &str) -> String {
        let params = json!({ "cwd": cwd, "mcpServers": [] });
        let (response, _) = self.request(req_id, "session/new", params).await;
        response["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("{response}"))
            .to_string()
    }

    /// Send `session/new` with the id `req_id` in `cwd`, and get the error of
    /// the response. Panics if the response has no error.
    /// Returns:
    ///   The error code and message.
    pub async fn session_error(&mut self, req_id: i64, cwd: &str) -> (i64, String) {
        let params = json!({ "cwd": cwd, "mcpServers": [] });
        let (response, _) = self.request(req_id, "session/new", params).await;
        let error = &response["error"];
        let code = error["code"]
            .as_i64()
            .unwrap_or_else(|| panic!("{response}"));
        (code, error["message"].as_str().unwrap().to_string())
    }

    /// Check if the proxy still runs.
    pub fn proxy_runs(&self) -> bool {
        !self.proxy.is_finished()
    }

    /// Close the input of the proxy, as an editor that exits.
    pub fn close(&mut self) {
        self.input = None;
    }

    /// Close the input and wait for the proxy end. Panics after 10
    /// seconds.
    /// Returns:
    ///   The exit code of the proxy.
    pub async fn exit_code(mut self) -> i32 {
        self.close();
        self.proxy_end().await
    }

    /// Wait for the proxy end, and keep the input open. Panics after 10
    /// seconds.
    /// Returns:
    ///   The exit code of the proxy.
    pub async fn proxy_end(self) -> i32 {
        tokio::time::timeout(Duration::from_secs(10), self.proxy)
            .await
            .expect("proxy end in time")
            .unwrap()
    }
}

/// Get the messages that the fake agent got, from the client messages
/// `msgs`.
pub fn seen(msgs: &[Value]) -> Vec<Value> {
    msgs.iter()
        .filter_map(|m| m.pointer("/params/seen").cloned())
        .collect()
}

/// Get the messages that the fake agent in `sandbox` got, from the client
/// messages `msgs`.
pub fn seen_from(msgs: &[Value], sandbox: &str) -> Vec<Value> {
    msgs.iter()
        .filter(|m| m.pointer("/params/from").and_then(Value::as_str) == Some(sandbox))
        .filter_map(|m| m.pointer("/params/seen").cloned())
        .collect()
}

/// Get the ids of the client requests in `seen` messages, in order.
pub fn request_ids(seen: &[Value]) -> Vec<Value> {
    seen.iter()
        .filter(|m| m.get("method").is_some() && m["id"].is_i64())
        .map(|m| m["id"].clone())
        .collect()
}
