//! State of one session in a shared agent process.
//!
//! Each session has its own identity for the proxy. It keeps its open
//! client requests, its editor MCP servers, its session dir in the guest
//! and its client messages that wait. A session that starts an ACP session
//! shows all notices of the sandbox in it. When the session ends, the
//! client gets an error for each open request, and a message in the
//! session if the agent exited. The client shows these messages as session
//! notices if it can, else as agent text.
//!
//! A session that does not start with a session request is one-shot. It
//! runs only its first request and owns no ACP sessions.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::super::proxy::{Claim, HandleId, HandleMessage};
use super::super::rpc::{self, Kind, RequestId, SessionId, key, session_id};
use super::super::{AgentProcessLauncher, Location, mcp};
use super::Listed;
use super::shared::StartFailure;
use crate::messages;
use crate::sandbox::NoticeReader;

/// One session of a shared agent process.
pub(super) struct Session {
    /// Identity of the session for the proxy.
    pub(super) hid: HandleId,
    /// Sandbox of the session dir of the session request.
    pub(super) location: Location,
    /// Session dir of the session request on the host.
    pub(super) cwd: String,
    messages: mpsc::Sender<(HandleId, HandleMessage)>,
    /// The client shows session notices.
    notices: bool,
    /// Map key of the id of the session request.
    first_key: RequestId,
    /// Method of the first request.
    first_method: String,
    /// Session id in the params of the first request, if it has one.
    first_sid: Option<SessionId>,
    /// The first request is not a session request. The session ends after
    /// its response and owns no ACP sessions.
    pub(super) one_shot: bool,
    /// The session request went to the agent.
    pub(super) first_sent: bool,
    /// Session id of a session request that loads or resumes a session.
    reload_sid: Option<SessionId>,
    /// The session that the session request started.
    claimed: Option<SessionId>,
    /// Ids of the client requests without a response, by map key.
    pub(super) open: HashMap<RequestId, Value>,
    /// Order numbers of the open `session/prompt` requests, by map key.
    prompts: HashMap<RequestId, u64>,
    /// Editor MCP servers of the session.
    pub(super) mcp: mcp::Registry,
    /// MCP server registration of a session request that runs. The client
    /// messages of the session wait until it completes.
    pub(super) registration: Option<mcp::Registration>,
    /// Client messages that wait for the handshake or the registration.
    pub(super) queue: VecDeque<Value>,
    /// Map keys of the open `session/list` requests. Their responses have
    /// guest session dirs.
    lists: HashSet<RequestId>,
    /// The proxy ended the session. It gets the exit code at the end of
    /// the process.
    pub(super) detach: Option<oneshot::Sender<i32>>,
}

impl Session {
    /// Make the session of the session request `first`. The request waits
    /// in the queue.
    /// Args:
    ///  - `hid`: Identity of the session for the proxy
    ///  - `location`: Sandbox of the session dir
    ///  - `cwd`: Session dir on the host
    ///  - `first`: The session request
    ///  - `messages`: Destination of the messages to the proxy
    ///  - `notices`: The client shows session notices
    pub(super) fn new(
        hid: HandleId,
        location: Location,
        cwd: String,
        first: Value,
        messages: mpsc::Sender<(HandleId, HandleMessage)>,
        notices: bool,
    ) -> Self {
        let method = first.get("method").and_then(Value::as_str);
        let first_method = method.unwrap_or_default().to_string();
        let one_shot = !matches!(
            method,
            Some("session/new" | "session/load" | "session/resume" | "session/fork")
        );
        let reload = matches!(method, Some("session/load" | "session/resume"));
        let first_sid = session_id(&first).map(str::to_string);
        let reload_sid = first_sid.clone().filter(|_| reload);
        Self {
            hid,
            location,
            cwd,
            messages,
            notices,
            first_key: key(first.get("id").unwrap_or(&Value::Null)),
            first_method,
            first_sid,
            one_shot,
            first_sent: false,
            reload_sid,
            claimed: None,
            open: HashMap::new(),
            prompts: HashMap::new(),
            mcp: mcp::Registry::default(),
            registration: None,
            queue: VecDeque::from([first]),
            lists: HashSet::new(),
            detach: None,
        }
    }

    /// Check if the one-shot session has its response: the first request
    /// went out and has no open request or registration.
    pub(super) fn one_shot_done(&self) -> bool {
        self.one_shot
            && self.first_sent
            && self.registration.is_none()
            && !self.open.contains_key(&self.first_key)
    }

    /// Order number of the last open `session/prompt`, if one is open.
    pub(super) fn last_prompt(&self) -> Option<u64> {
        self.prompts.values().copied().max()
    }

    /// Handle the agent response `msg` to the client request `req_id` of
    /// this session. Changes the session dirs of a `session/list` reply to
    /// host paths.
    /// Returns:
    ///   The session that the response starts, if it is the OK response to
    ///   the session request.
    pub(super) fn on_response(&mut self, req_id: &Value, msg: &mut Value) -> Option<Claim> {
        let id_key = key(req_id);
        self.mcp.on_response(req_id, msg);
        self.open.remove(&id_key);
        self.prompts.remove(&id_key);
        if self.lists.remove(&id_key) {
            let location = &self.location;
            rpc::map_session_list(msg, &location.host_root, &location.guest_root);
        }
        if self.one_shot || id_key != self.first_key || msg.get("error").is_some() {
            return None;
        }
        let replace = self.reload_sid.is_some();
        let sid = msg
            .pointer("/result/sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.reload_sid.clone())?;
        self.claimed = Some(sid.clone());
        Some(Claim {
            session: sid,
            replace,
        })
    }

    /// Keep the listed sessions of the response `msg` to the one-shot
    /// request in `listed`.
    /// Returns:
    ///   `true` if `msg` is the response to the one-shot request.
    pub(super) fn on_one_shot_response(
        &self,
        req_id: &Value,
        msg: &Value,
        listed: &mut Listed,
    ) -> bool {
        if !self.one_shot || key(req_id) != self.first_key {
            return false;
        }
        let sid = self.first_sid.as_deref();
        listed.record(&self.first_method, &self.cwd, sid, msg);
        true
    }

    /// Keep the client request `req_id` as open until its response.
    /// Args:
    ///  - `method`: Method of the request
    ///  - `params`: Params of the request
    ///  - `next_prompt`: Order number of the last prompt of the agent.
    ///    A prompt increments it.
    pub(super) fn track(
        &mut self,
        req_id: Value,
        method: &str,
        params: &Value,
        next_prompt: &mut u64,
    ) {
        let id_key = key(&req_id);
        match method {
            "session/close" => self.mcp.track_close(&req_id, params),
            "session/list" => {
                self.lists.insert(id_key.clone());
            }
            "session/prompt" => {
                *next_prompt += 1;
                self.prompts.insert(id_key.clone(), *next_prompt);
            }
            _ => {}
        }
        self.open.insert(id_key, req_id);
    }

    /// Get the guest path of the session dir `cwd`.
    /// Returns:
    ///   The guest path, or the error code and message if `cwd` is not an
    ///   absolute path or is not in the sandbox of the session.
    pub(super) async fn guest_cwd<S: AgentProcessLauncher>(
        &self,
        launcher: &S,
        cwd: &str,
    ) -> Result<String, (i64, String)> {
        if cwd == self.cwd {
            return Ok(self.location.guest_cwd.clone());
        }
        if !cwd.starts_with('/') {
            return Err((rpc::INVALID_PARAMS, messages::acp::cwd_not_absolute(cwd)));
        }
        let location = launcher
            .locate(cwd)
            .await
            .map_err(|e| (rpc::INTERNAL_ERROR, format!("{e:#}")))?;
        // One agent runs in one sandbox. It cannot see the files of an other
        // sandbox.
        if location.sandbox != self.location.sandbox {
            return Err((rpc::INVALID_PARAMS, messages::acp::other_sandbox(cwd)));
        }
        Ok(location.guest_cwd)
    }

    /// Tell the client about the session end: an error for each open and
    /// waiting request, and a session message if `notify` is set.
    /// Args:
    ///  - `error`: Error message for the open requests
    ///  - `failed`: Error for the session request if the agent start
    ///    failed before the agent got it
    ///  - `notify`: Sandbox and exit code for the session message about
    ///    the agent exit, or `None` for no message
    pub(super) async fn end(
        &mut self,
        error: &str,
        failed: Option<&StartFailure>,
        notify: Option<(&str, i32)>,
    ) {
        for msg in std::mem::take(&mut self.queue) {
            let Some(Kind::Request { id: req_id, .. }) = rpc::kind(&msg) else {
                continue;
            };
            if let Some(failed) = failed
                && !self.first_sent
                && key(req_id) == self.first_key
            {
                self.to_client(failed.response(req_id), None).await;
            } else {
                self.open.insert(key(req_id), req_id.clone());
            }
        }
        self.prompts.clear();
        for (_, req_id) in std::mem::take(&mut self.open) {
            self.reply(&req_id, (rpc::INTERNAL_ERROR, error.to_string()))
                .await;
        }
        let (Some(sid), Some((sandbox, code))) = (self.claimed.clone(), notify) else {
            return;
        };
        let detail = messages::acp::agent_stopped_detail(sandbox, code);
        self.notice(&sid, "error", messages::acp::AGENT_STOPPED, &detail)
            .await;
    }

    /// Tell the client about all notices of the sandbox, one warning for
    /// each, in the ACP session that the session request started. Does
    /// nothing if the session has no ACP session.
    /// Args:
    ///  - `reader`: Reader of the sandbox notices
    pub(super) fn show_notices(&self, reader: NoticeReader) {
        let Some(sid) = self.claimed.clone() else {
            return;
        };
        let (hid, tx, notices) = (self.hid, self.messages.clone(), self.notices);
        // The read goes to the sandbox. The other sessions of the agent
        // must not wait for it. The warnings come after the response,
        // because the response is in the queue before the read starts.
        tokio::task::spawn_local(async move {
            for notice in reader.read().await {
                let msg = notice_message(
                    notices,
                    &sid,
                    "warning",
                    messages::acp::SANDBOX_NOTICE,
                    &notice.text,
                );
                let claim = None;
                let _ = tx.send((hid, HandleMessage::Client { msg, claim })).await;
            }
        });
    }

    /// Send a message to the client in the ACP session `sid` (see
    /// [`notice_message`]).
    async fn notice(&self, sid: &str, severity: &str, title: &str, detail: &str) {
        let msg = notice_message(self.notices, sid, severity, title, detail);
        self.to_client(msg, None).await;
    }

    /// Send the error `error` for the client request `req_id`.
    pub(super) async fn reply(&self, req_id: &Value, error: (i64, String)) {
        let msg = rpc::error_response(req_id, error.0, &error.1);
        self.to_client(msg, None).await;
    }

    /// Send a message to the client through the proxy.
    pub(super) async fn to_client(&self, msg: Value, claim: Option<Claim>) {
        self.send(HandleMessage::Client { msg, claim }).await;
    }

    /// Send a message about the session to the proxy.
    pub(super) async fn send(&self, message: HandleMessage) {
        let _ = self.messages.send((self.hid, message)).await;
    }
}

/// Make a message to the client in the ACP session `sid`: a session notice
/// with `severity` if the client shows notices, else agent text.
/// Args:
///  - `notices`: The client shows session notices
///  - `title`, `detail`: Title and text of the message
fn notice_message(notices: bool, sid: &str, severity: &str, title: &str, detail: &str) -> Value {
    let update = if notices {
        json!({
            "sessionUpdate": "notice", "severity": severity,
            "title": title, "description": detail,
        })
    } else {
        json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": format!("{title}: {detail}") },
        })
    };
    let params = json!({ "sessionId": sid, "update": update });
    json!({ "jsonrpc": "2.0", "method": "session/update", "params": params })
}
