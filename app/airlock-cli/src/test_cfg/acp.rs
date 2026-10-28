//! Helpers for the ACP proxy tests: a fake agent that runs as a local
//! process, fake sandboxes, a fake sandbox that adds the editor MCP
//! servers, and a client that talks to the proxy.

use std::cell::{Cell, RefCell};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::cli_capnp::cli_service;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::acp::{
    AGENT_EVENTS, AGENT_INPUT_LINES, AgentProcess, AgentStarter, Control, Location, ProxyConfig,
};
use crate::rpc::ProcessEvent;
use crate::runtime::SignalStream;
use crate::test_cfg::mcp_grant::{FakeGrant, GrantEvent, Route};

/// Fake ACP agent. It writes the lines of `$AGENT_PRE`, replies to the
/// proxy handshake with `$AGENT_INIT` and then writes the lines of
/// `$AGENT_SAY`. For each other input line, it writes a `session/update`
/// notification with the line in `params.seen`. It replies to each request
/// with a numeric id with an empty result, or with the error
/// `$AGENT_ERROR`. Other settings:
///  - `AGENT_NO_INIT`: Never reply to the handshake
///  - `AGENT_STDERR`: Number of bytes to write to stderr before the
///    handshake reply
///  - `AGENT_FLOOD`: After the handshake, write one line without end and
///    read no more input
///  - `AGENT_EOF`: Exit code at the end of the input, or `ignore` to wait
///    for the kill
///  - `AGENT_SESSION_IDS`: Reply to `session/new` and `session/fork` with
///    the result `{"sessionId":"s<N>"}`, where N counts from 1
///
/// The notification `test/exit` makes it write a `session/update`
/// notification with `params.last` and no newline, and exit with code 7.
/// At the end of its input, it writes the same notification.
pub const FAKE_AGENT: &str = r#"
last() { printf '{"jsonrpc":"2.0","method":"session/update","params":{"last":true}}'; }
seen() { printf '{"jsonrpc":"2.0","method":"session/update","params":{"seen":%s}}\n' "$1"; }
while IFS= read -r line; do
  case "$line" in
  *'"id":"airlock-acp-initialize"'*)
    [ -n "$AGENT_PRE" ] && printf '%s\n' "$AGENT_PRE"
    [ -n "$AGENT_NO_INIT" ] && continue
    [ -n "$AGENT_STDERR" ] && head -c "$AGENT_STDERR" /dev/zero >&2
    printf '{"jsonrpc":"2.0","id":"airlock-acp-initialize","result":%s}\n' "$AGENT_INIT"
    [ -n "$AGENT_SAY" ] && printf '%s\n' "$AGENT_SAY"
    if [ -n "$AGENT_FLOOD" ]; then
      while :; do printf %s xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done
    fi
    seen "$line" ;;
  *'"method":"test/exit"'*) last; exit 7 ;;
  *)
    seen "$line"
    case "$line" in
    *'"method"'*'"id":'[0-9]*|*'"id":'[0-9]*'"method"'*)
      id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      if [ -n "$AGENT_ERROR" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":%s}\n' "$id" "$AGENT_ERROR"
      else
        result='{}'
        if [ -n "$AGENT_SESSION_IDS" ]; then
          case "$line" in
          *'"method":"session/new"'*|*'"method":"session/fork"'*)
            n=$((${n:-0} + 1)); result="{\"sessionId\":\"s$n\"}" ;;
          esac
        fi
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
        },
    })
}

/// Agent starter for tests. Runs the agent as a local process. Has two
/// fake running sandboxes: `a` for `/projects/a` and `b` for
/// `/projects/b`, both with the guest project dir `/work`.
pub struct LocalStarter {
    /// Environment variables of the agent.
    env: Vec<(String, String)>,
    /// Number of agent starts.
    pub starts: Rc<Cell<usize>>,
    /// Sandbox names and their project dirs on the host.
    roots: Vec<(String, String)>,
    /// Fake sandbox that adds the editor MCP servers.
    registrar: Option<cli_service::Client>,
}

impl LocalStarter {
    /// Make a starter that gives the agent the environment `env`. The
    /// agent gets the [`full_agent_init`] if `env` has no `AGENT_INIT`.
    pub fn new(env: &[(&str, &str)]) -> Self {
        let mut env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if !env.iter().any(|(k, _)| k == "AGENT_INIT") {
            env.push(("AGENT_INIT".into(), full_agent_init().to_string()));
        }
        Self {
            env,
            starts: Rc::default(),
            roots: ["a", "b"]
                .iter()
                .map(|s| ((*s).into(), format!("/projects/{s}")))
                .collect(),
            registrar: None,
        }
    }

    /// Use the host dir `root` as the project dir of sandbox `a`.
    pub fn with_root(mut self, root: &str) -> Self {
        self.roots[0].1 = root.to_string();
        self
    }

    /// Give the agents the fake sandbox `registrar` for the editor MCP
    /// servers.
    pub fn with_registrar(mut self, registrar: cli_service::Client) -> Self {
        self.registrar = Some(registrar);
        self
    }
}

impl AgentStarter for LocalStarter {
    async fn locate(&self, cwd: &str) -> anyhow::Result<Location> {
        for (sandbox, root) in &self.roots {
            let Some(rest) = cwd.strip_prefix(root.as_str()) else {
                continue;
            };
            if rest.is_empty() || rest.starts_with('/') {
                return Ok(Location {
                    sandbox: sandbox.clone(),
                    guest_cwd: format!("/work{rest}"),
                });
            }
        }
        anyhow::bail!("no running sandbox for {cwd}")
    }

    async fn start(&self, cwd: &str, argv: &[String]) -> anyhow::Result<(Location, AgentProcess)> {
        let location = self.locate(cwd).await?;
        self.starts.set(self.starts.get() + 1);
        let mut agent = spawn_agent(argv, &self.env)?;
        agent.mcp = self.registrar.clone();
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
            let signum = match request {
                Control::Kill => libc::SIGKILL,
                Control::Signal(signum) => signum,
            };
            // SAFETY: sends a signal to the child process only.
            unsafe { libc::kill(pid, signum) };
        }
    });

    Ok(AgentProcess {
        input,
        events,
        control,
        mcp: None,
    })
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
    ///   The service for [`LocalStarter::with_registrar`], and its test
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
pub fn test_config(restrict: bool) -> ProxyConfig {
    ProxyConfig {
        argv: vec!["sh".into(), "-c".into(), FAKE_AGENT.into()],
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
pub fn mcp_config(restrict: bool) -> ProxyConfig {
    ProxyConfig {
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
}

impl TestClient {
    /// Run the proxy with `starter` and `config` in a task. Call inside a
    /// `LocalSet`.
    pub fn start(starter: LocalStarter, config: ProxyConfig) -> Self {
        let (input, proxy_input) = tokio::io::duplex(1 << 20);
        let (proxy_output, output) = tokio::io::duplex(1 << 20);
        let (proxy_stderr, stderr) = tokio::io::duplex(64 * 1024);
        let proxy = tokio::task::spawn_local(async move {
            let input = BufReader::new(proxy_input);
            crate::acp::serve(&starter, &config, input, proxy_output, proxy_stderr)
                .await
                .unwrap()
        });
        Self {
            input: Some(input),
            output: BufReader::new(output).lines(),
            _stderr: stderr,
            proxy,
        }
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

    /// Send a request and wait for its response.
    /// Returns:
    ///   The response, and the other messages that came before it.
    pub async fn request(&mut self, id: i64, method: &str, params: Value) -> (Value, Vec<Value>) {
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&msg).await;
        self.response(id).await
    }

    /// Wait for the response to the request `id`.
    /// Returns:
    ///   The response, and the other messages that came before it.
    pub async fn response(&mut self, id: i64) -> (Value, Vec<Value>) {
        let mut others = Vec::new();
        loop {
            let msg = self.recv().await;
            if msg.get("method").is_none() && msg["id"] == json!(id) {
                return (msg, others);
            }
            others.push(msg);
        }
    }

    /// Do the client handshake with the client features `caps`.
    /// Returns:
    ///   The `initialize` result of the proxy.
    pub async fn initialize(&mut self, caps: Value) -> Value {
        let params = json!({ "protocolVersion": 1, "clientCapabilities": caps });
        let (response, _) = self.request(0, "initialize", params).await;
        response["result"].clone()
    }

    /// Start the proxy with `starter` and `config`, do the client handshake
    /// and start a session in `cwd`. Panics if the session fails.
    pub async fn session(starter: LocalStarter, config: ProxyConfig, cwd: &str) -> Self {
        let mut client = Self::start(starter, config);
        client.initialize(json!({})).await;
        let params = json!({ "cwd": cwd, "mcpServers": [] });
        let (response, _) = client.request(2, "session/new", params).await;
        assert!(response.get("result").is_some(), "{response}");
        client
    }

    /// Send `session/new` with the id `id` in `cwd`, and get the error of
    /// the response. Panics if the response has no error.
    /// Returns:
    ///   The error code and message.
    pub async fn session_error(&mut self, id: i64, cwd: &str) -> (i64, String) {
        let params = json!({ "cwd": cwd, "mcpServers": [] });
        let (response, _) = self.request(id, "session/new", params).await;
        let error = &response["error"];
        let code = error["code"]
            .as_i64()
            .unwrap_or_else(|| panic!("{response}"));
        (code, error["message"].as_str().unwrap().to_string())
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
