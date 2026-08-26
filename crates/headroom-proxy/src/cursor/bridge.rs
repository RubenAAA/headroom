//! The tool bridge: Claude Code's tools, offered to Cursor's agent over MCP.
//!
//! Cursor's agent will not emit a tool call for a tool it cannot see, and it
//! has no way to see Claude Code's. The one channel that crosses the process
//! boundary is MCP, so the proxy serves an MCP endpoint per conversation whose
//! `tools/list` is exactly the `tools` array of the request in flight.
//!
//! What makes it work is that an MCP call may take its time. Measured against
//! a live agent on 2026-08-26: a `tools/call` held open for 75 seconds still
//! completed and the agent carried on. So the proxy does not answer the call.
//! It *parks* it — holds the JSON-RPC request open, hands the tool up to Claude
//! Code as an ordinary `tool_use` block, ends the HTTP response, and leaves the
//! agent blocked. When the next request arrives carrying the `tool_result`, the
//! parked call is answered and the same agent process picks up where it was.
//!
//! The subprocess therefore outlives the HTTP request that started it, which is
//! the one structural difference from every other route in the proxy.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{mpsc, oneshot, Mutex};

/// A `tools/call` held open, on its way up to the turn loop.
///
/// The channel that releases it is not in here. It lives in the session's
/// waiting map, keyed by `id`, because the thing that answers a call is the
/// `tool_result` in some later request, not whoever happened to read this.
#[derive(Debug, Clone)]
pub(crate) struct ParkedCall {
    /// The id given to the Anthropic `tool_use` block, and the one the matching
    /// `tool_result` will carry back.
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) args: Value,
}

/// What Claude Code made of the tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolOutcome {
    Ok(String),
    /// The tool failed, or the user declined it. Cursor is told either way, so
    /// the model can react instead of waiting on a call that is never coming.
    Failed(String),
}

impl ToolOutcome {
    fn to_mcp_result(&self) -> Value {
        match self {
            Self::Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
            Self::Failed(text) => {
                json!({"content": [{"type": "text", "text": text}], "isError": true})
            }
        }
    }
}

/// One conversation's worth of bridge state.
pub(crate) struct Session {
    /// Exactly what the request in flight advertised. Replaced each turn:
    /// Claude Code varies the set — a skill or a plan-mode switch changes it —
    /// and a stale list would offer the model a tool that is no longer there.
    tools: Mutex<Vec<Value>>,
    /// Parked calls, on their way up to the turn loop.
    outbox: mpsc::UnboundedSender<ParkedCall>,
    /// One sender per call still in flight, keyed by `tool_use` id. Sending on
    /// it releases the blocked MCP request.
    waiting: Mutex<HashMap<String, oneshot::Sender<ToolOutcome>>>,
    /// Cursor's chat id, once a turn has reported one. `--resume` takes this,
    /// and it is what keeps the history on Cursor's side instead of ours.
    chat_id: Mutex<Option<String>>,
    /// Monotonic within a session — enough to make a `tool_use` id unique.
    next_call: AtomicU64,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

impl Session {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<ParkedCall>) {
        let (outbox, inbox) = mpsc::unbounded_channel();
        let session = Arc::new(Self {
            tools: Mutex::new(Vec::new()),
            outbox,
            waiting: Mutex::new(HashMap::new()),
            chat_id: Mutex::new(None),
            next_call: AtomicU64::new(0),
        });
        (session, inbox)
    }

    pub(crate) async fn set_tools(&self, tools: Vec<Value>) {
        *self.tools.lock().await = tools;
    }

    pub(crate) async fn chat_id(&self) -> Option<String> {
        self.chat_id.lock().await.clone()
    }

    pub(crate) async fn set_chat_id(&self, id: String) {
        *self.chat_id.lock().await = Some(id);
    }

    /// Whether any call is still parked. The turn loop uses this to tell a
    /// conversation that is mid-tool from one that is finished.
    // Introspection for a bridge-status surface that is not wired yet.
    #[allow(dead_code)]
    pub(crate) async fn has_parked_calls(&self) -> bool {
        !self.waiting.lock().await.is_empty()
    }

    /// The MCP view of the advertised tools.
    ///
    /// Anthropic calls the schema `input_schema`; MCP calls the same JSON
    /// Schema `inputSchema`. That rename is the entire translation.
    async fn mcp_tools(&self) -> Vec<Value> {
        self.tools
            .lock()
            .await
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.as_str()?;
                Some(json!({
                    "name": name,
                    "description": tool.get("description").and_then(Value::as_str).unwrap_or(""),
                    "inputSchema": tool
                        .get("input_schema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                }))
            })
            .collect()
    }

    /// Park a call and wait for Claude Code to answer it.
    ///
    /// The `await` at the end is the whole mechanism. It holds the agent's HTTP
    /// request open while this turn ends and the next one begins, which is what
    /// lets a tool run in a different process from the model that asked for it.
    async fn park(&self, name: &str, args: Value) -> ToolOutcome {
        let n = self.next_call.fetch_add(1, Ordering::Relaxed);
        let id = format!("toolu_cursor_{n:08}");
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().await.insert(id.clone(), tx);

        let call = ParkedCall {
            id: id.clone(),
            name: name.to_string(),
            args,
        };
        if self.outbox.send(call).is_err() {
            // No turn loop is listening, so nothing will ever answer this.
            self.waiting.lock().await.remove(&id);
            return ToolOutcome::Failed("no turn is in flight to run this tool".into());
        }

        match rx.await {
            Ok(outcome) => outcome,
            // The session was dropped out from under the call.
            Err(_) => ToolOutcome::Failed("the host abandoned the tool call".into()),
        }
    }

    /// Answer a parked call. `false` when nothing was waiting on that id — a
    /// `tool_result` for a call this session never made, which is what a
    /// transcript replayed from before a restart looks like.
    pub(crate) async fn answer(&self, id: &str, outcome: ToolOutcome) -> bool {
        let Some(tx) = self.waiting.lock().await.remove(id) else {
            return false;
        };
        tx.send(outcome).is_ok()
    }
}

/// How long a conversation may sit parked before it is reaped.
///
/// A parked driver is a live `cursor-agent` process and a scratch directory,
/// waiting on a `tool_result` that may never come — the user hits escape, the
/// client crashes, the turn is abandoned. Nothing else would ever remove it, so
/// without a deadline the process count only goes up.
///
/// Thirty minutes because the two failure modes are not symmetric. Reaping too
/// late costs one idle process and a temp directory. Reaping too early kills a
/// conversation whose tool was legitimately slow — a full build or test run
/// through `Bash` — and the user sees a working session die for no visible
/// reason. So the deadline is set well past any tool anyone is waiting on
/// rather than close to it.
pub const MAX_PARK: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Most conversations that may be parked at once.
///
/// A second bound, on a different axis from [`MAX_PARK`]: the deadline caps how
/// long one process lives, this caps how many exist. A client opening turns
/// faster than it finishes them would otherwise stay under the deadline the
/// whole way up.
pub(crate) const MAX_PARKED: usize = 32;

struct Parked {
    driver: super::turn::Conversation,
    since: std::time::Instant,
}

/// Every live conversation, keyed the way the proxy keys conversations.
///
/// Holds two things per key, and the second is the awkward one. A `Session` is
/// just state. A *driver* is a running agent process, and it has to outlive the
/// HTTP request that created it: when the model reaches for a tool the response
/// ends, but the process must stay alive and blocked until a later request
/// brings the result. So the driver is parked here between requests rather than
/// owned by whoever is streaming, which would kill it on drop.
///
/// Both maps stay empty unless a `cursor:` route is configured, and nothing
/// outside `crate::cursor` reads either one — worth knowing before suspecting
/// this of anything happening on the Anthropic path.
///
/// The two locks are never held at once. Every site takes one, finishes with
/// it, and only then takes the other, which is why `close` (sessions then
/// drivers) and `reap_idle` (drivers then sessions) can disagree about the
/// order without deadlocking. Keep it that way: a future edit that holds one
/// across the other reintroduces the cycle.
#[derive(Default)]
pub struct Bridge {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    drivers: Mutex<HashMap<String, Parked>>,
}

impl Bridge {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Start a conversation, replacing any session already under that key.
    ///
    /// Returns the inbox as well, and only here: a session with no reader would
    /// park calls that nothing can answer, so the caller that takes the inbox
    /// is by construction the one that must drive the turn.
    pub(crate) async fn open(&self, key: &str) -> (Arc<Session>, mpsc::UnboundedReceiver<ParkedCall>) {
        let (session, inbox) = Session::new();
        self.sessions
            .lock()
            .await
            .insert(key.to_string(), session.clone());
        (session, inbox)
    }

    pub(crate) async fn get(&self, key: &str) -> Option<Arc<Session>> {
        self.sessions.lock().await.get(key).cloned()
    }

    /// Park the driver until the next request comes for it.
    pub(crate) async fn park_driver(&self, key: &str, driver: super::turn::Conversation) {
        let evicted = {
            let mut drivers = self.drivers.lock().await;
            drivers.insert(
                key.to_string(),
                Parked {
                    driver,
                    since: std::time::Instant::now(),
                },
            );
            // Over the cap, the oldest goes. Chosen over refusing the new one
            // because the newest park is the conversation someone is currently
            // waiting on, and the oldest is the likeliest to be abandoned.
            if drivers.len() > MAX_PARKED {
                let oldest = drivers
                    .iter()
                    .min_by_key(|(_, parked)| parked.since)
                    .map(|(k, _)| k.clone());
                oldest.and_then(|k| drivers.remove(&k).map(|p| (k, p)))
            } else {
                None
            }
        };
        // Reaped outside the lock: shutting a driver down waits on a process,
        // and holding the map while that happens stalls every other
        // conversation.
        if let Some((evicted_key, mut parked)) = evicted {
            tracing::warn!(
                event = "cursor_driver_evicted",
                conversation = %evicted_key,
                parked_for_s = parked.since.elapsed().as_secs(),
                limit = MAX_PARKED,
                "too many parked conversations; killing the oldest"
            );
            parked.driver.shutdown().await;
        }
        self.reap_idle(MAX_PARK).await;
    }

    /// Take the parked driver, if this conversation has one waiting.
    pub(crate) async fn take_driver(&self, key: &str) -> Option<super::turn::Conversation> {
        self.drivers.lock().await.remove(key).map(|p| p.driver)
    }

    /// Kill every conversation parked longer than `max_park`. Returns how many.
    ///
    /// Called whenever a driver is parked, so a busy proxy sweeps itself, and
    /// on a timer from `main`, so an idle one does too. Both are needed: the
    /// opportunistic sweep never fires on a proxy that has gone quiet, which is
    /// exactly when a forgotten process would sit longest.
    pub async fn reap_idle(&self, max_park: std::time::Duration) -> usize {
        let expired: Vec<(String, Parked)> = {
            let mut drivers = self.drivers.lock().await;
            let keys: Vec<String> = drivers
                .iter()
                .filter(|(_, parked)| parked.since.elapsed() > max_park)
                .map(|(k, _)| k.clone())
                .collect();
            keys.into_iter()
                .filter_map(|k| drivers.remove(&k).map(|p| (k, p)))
                .collect()
        };
        let reaped = expired.len();
        for (key, mut parked) in expired {
            tracing::warn!(
                event = "cursor_driver_reaped",
                conversation = %key,
                parked_for_s = parked.since.elapsed().as_secs(),
                "a conversation was abandoned mid-tool; killing its agent"
            );
            parked.driver.shutdown().await;
            self.sessions.lock().await.remove(&key);
        }
        reaped
    }

    /// Forget the conversation and kill any agent still running for it.
    pub(crate) async fn close(&self, key: &str) {
        self.sessions.lock().await.remove(key);
        let parked = self.drivers.lock().await.remove(key);
        if let Some(mut parked) = parked {
            parked.driver.shutdown().await;
        }
    }

    // Introspection for a bridge-status surface that is not wired yet.
    #[allow(dead_code)]
    pub(crate) async fn live_sessions(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Conversations parked mid-tool. Every one is a live agent process, so
    /// this is the number worth watching.
    // Same unwired status surface as the two above.
    #[allow(dead_code)]
    pub(crate) async fn parked_drivers(&self) -> usize {
        self.drivers.lock().await.len()
    }
}

/// Serve one JSON-RPC request against a session.
///
/// Returns `None` for a notification, which takes no reply.
pub(crate) async fn handle_rpc(session: &Session, request: &Value) -> Option<Value> {
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let id = request.get("id").cloned();

    if method.starts_with("notifications/") {
        return None;
    }

    let result = match method {
        "initialize" => Ok(json!({
            // Echo the client's version. MCP negotiates by agreement, and
            // Cursor is the only client here.
            "protocolVersion": request
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "headroom", "version": env!("CARGO_PKG_VERSION")},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": session.mcp_tools().await})),
        "tools/call" => {
            let name = request
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if name.is_empty() {
                Err((-32602, "tools/call without a name".to_string()))
            } else {
                let args = request
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                Ok(session.park(&name, args).await.to_mcp_result())
            }
        }
        other => Err((-32601, format!("no method {other}"))),
    };

    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic_tool(name: &str) -> Value {
        json!({
            "name": name,
            "description": format!("the {name} tool"),
            "input_schema": {
                "type": "object",
                "required": ["file_path"],
                "properties": {"file_path": {"type": "string"}},
            },
        })
    }

    fn rpc(method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    }

    #[tokio::test]
    async fn initialize_echoes_the_clients_protocol_version() {
        let (session, _inbox) = Bridge::new().open("c1").await;
        let reply = handle_rpc(&session, &rpc("initialize", json!({"protocolVersion": "2025-06-18"})))
            .await
            .expect("initialize is answered");
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert!(reply["result"]["capabilities"]["tools"].is_object());
    }

    /// A notification has no id and must draw no reply. Answering one puts a
    /// stray response on the wire that the client cannot match to anything.
    #[tokio::test]
    async fn a_notification_is_not_answered() {
        let (session, _inbox) = Bridge::new().open("c1").await;
        let out = handle_rpc(&session, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
        assert!(out.is_none());
    }

    /// The rename is the whole translation, and getting it wrong means the
    /// model sees a tool with no arguments and calls it with none.
    #[tokio::test]
    async fn anthropic_tools_are_republished_under_the_mcp_schema_key() {
        let (session, _inbox) = Bridge::new().open("c1").await;
        session.set_tools(vec![anthropic_tool("Read"), anthropic_tool("Bash")]).await;

        let reply = handle_rpc(&session, &rpc("tools/list", json!({}))).await.unwrap();
        let tools = reply["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "Read");
        assert_eq!(tools[0]["inputSchema"]["required"][0], "file_path");
        assert!(tools[0].get("input_schema").is_none(), "the Anthropic key must not leak");
    }

    /// Claude Code varies its tool set between turns. A list from last turn
    /// would offer a tool that is no longer there.
    #[tokio::test]
    async fn the_tool_list_is_replaced_each_turn_not_appended_to() {
        let (session, _inbox) = Bridge::new().open("c1").await;
        session.set_tools(vec![anthropic_tool("Read"), anthropic_tool("Bash")]).await;
        session.set_tools(vec![anthropic_tool("Read")]).await;

        let reply = handle_rpc(&session, &rpc("tools/list", json!({}))).await.unwrap();
        assert_eq!(reply["result"]["tools"].as_array().unwrap().len(), 1);
    }

    /// The heart of it: `tools/call` must not return until someone answers,
    /// and the call must surface on the inbox while it waits.
    #[tokio::test]
    async fn a_tool_call_parks_until_it_is_answered() {
        let (session, mut inbox) = Bridge::new().open("c1").await;
        session.set_tools(vec![anthropic_tool("Read")]).await;

        let calling = {
            let session = session.clone();
            tokio::spawn(async move {
                handle_rpc(&session, &rpc("tools/call", json!({"name": "Read", "arguments": {"file_path": "/tmp/x"}}))).await
            })
        };

        let parked = inbox.recv().await.expect("the call reaches the turn loop");
        assert_eq!(parked.name, "Read");
        assert_eq!(parked.args["file_path"], "/tmp/x");
        assert!(parked.id.starts_with("toolu_"), "the id must be usable as a tool_use id");

        assert!(!calling.is_finished(), "the call must still be waiting");
        assert!(session.has_parked_calls().await);

        assert!(session.answer(&parked.id, ToolOutcome::Ok("file contents".into())).await);
        let reply = calling.await.expect("join").expect("answered");
        assert_eq!(reply["result"]["content"][0]["text"], "file contents");
        assert!(reply["result"].get("isError").is_none());
        assert!(!session.has_parked_calls().await, "answering clears the park");
    }

    /// A declined or failed tool has to come back as an MCP error, or the model
    /// waits on a call that already resolved.
    #[tokio::test]
    async fn a_failed_tool_comes_back_as_an_mcp_error() {
        let (session, mut inbox) = Bridge::new().open("c1").await;
        let calling = {
            let session = session.clone();
            tokio::spawn(async move {
                handle_rpc(&session, &rpc("tools/call", json!({"name": "Bash", "arguments": {}}))).await
            })
        };
        let parked = inbox.recv().await.unwrap();
        session.answer(&parked.id, ToolOutcome::Failed("the user declined".into())).await;

        let reply = calling.await.unwrap().unwrap();
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(reply["result"]["content"][0]["text"], "the user declined");
    }

    /// Several tools in one turn is normal. Each has to come back to its own
    /// caller, so answering out of order must still land correctly.
    #[tokio::test]
    async fn concurrent_calls_are_answered_by_id_not_by_order() {
        let (session, mut inbox) = Bridge::new().open("c1").await;
        let spawn_call = |name: &'static str| {
            let session = session.clone();
            tokio::spawn(async move {
                handle_rpc(&session, &rpc("tools/call", json!({"name": name, "arguments": {}}))).await
            })
        };
        let first = spawn_call("Read");
        let second = spawn_call("Bash");

        let a = inbox.recv().await.unwrap();
        let b = inbox.recv().await.unwrap();
        assert_ne!(a.id, b.id, "ids must be distinct or answers cross");

        // Answer the second one first.
        session.answer(&b.id, ToolOutcome::Ok(format!("answer for {}", b.name))).await;
        session.answer(&a.id, ToolOutcome::Ok(format!("answer for {}", a.name))).await;

        let by_name = |r: Value, name: &str| {
            assert_eq!(r["result"]["content"][0]["text"], format!("answer for {name}"));
        };
        by_name(first.await.unwrap().unwrap(), "Read");
        by_name(second.await.unwrap().unwrap(), "Bash");
    }

    /// Claude Code replays transcripts. A `tool_result` for a call this
    /// process never parked must be shrugged off, not panic or block.
    #[tokio::test]
    async fn answering_an_unknown_id_is_reported_not_fatal() {
        let (session, _inbox) = Bridge::new().open("c1").await;
        assert!(!session.answer("toolu_cursor_00000099", ToolOutcome::Ok("x".into())).await);
    }

    /// With no turn in flight nothing can ever answer, so the call has to fail
    /// fast rather than hold an agent process open forever.
    #[tokio::test]
    async fn a_call_with_no_turn_listening_fails_instead_of_hanging() {
        let (session, inbox) = Bridge::new().open("c1").await;
        drop(inbox);
        let reply = handle_rpc(&session, &rpc("tools/call", json!({"name": "Read", "arguments": {}})))
            .await
            .unwrap();
        assert_eq!(reply["result"]["isError"], true);
    }

    #[tokio::test]
    async fn an_unknown_method_is_a_jsonrpc_error_not_a_panic() {
        let (session, _inbox) = Bridge::new().open("c1").await;
        let reply = handle_rpc(&session, &rpc("resources/list", json!({}))).await.unwrap();
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn sessions_are_kept_and_dropped_by_key() {
        let bridge = Bridge::new();
        let (_s, _i) = bridge.open("c1").await;
        let (_s2, _i2) = bridge.open("c2").await;
        assert_eq!(bridge.live_sessions().await, 2);
        assert!(bridge.get("c1").await.is_some());
        bridge.close("c1").await;
        assert!(bridge.get("c1").await.is_none());
        assert_eq!(bridge.live_sessions().await, 1);
    }

    #[tokio::test]
    async fn the_chat_id_survives_so_the_next_turn_can_resume() {
        let (session, _inbox) = Bridge::new().open("c1").await;
        assert!(session.chat_id().await.is_none());
        session.set_chat_id("cf8812c0".into()).await;
        assert_eq!(session.chat_id().await.as_deref(), Some("cf8812c0"));
    }

    use super::super::agent::{spawn, AgentTurn, Workspace};
    use super::super::turn::Conversation;

    /// A driver whose agent sits doing nothing, so it can be parked and reaped
    /// without the test depending on a real CLI.
    async fn idle_driver(bridge: &Bridge, key: &str) -> Conversation {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("stub");
        std::fs::write(&stub, "#!/bin/sh\ncat > /dev/null\nsleep 300\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let (session, inbox) = bridge.open(key).await;
        let running = spawn(
            stub.to_str().unwrap(),
            &AgentTurn {
                model: "m".into(),
                workspace: dir.path().to_path_buf(),
                resume: None,
                prompt: "hi".into(),
                mcp_url: None,
                timeout: None,
                read_only: true,
            },
        )
        .await
        .expect("spawn");
        // Safe to drop now: the child is already exec'd, and on Unix the kernel
        // holds the inode open whatever happens to the directory entry.
        drop(dir);
        Conversation::new(session, inbox, running, Workspace::create(None).expect("workspace"))
    }

    /// A conversation abandoned mid-tool leaves a live agent process behind.
    /// Nothing else removes it, so without this the process count only climbs.
    #[tokio::test]
    async fn a_conversation_abandoned_mid_tool_is_reaped() {
        let bridge = Bridge::new();
        let driver = idle_driver(&bridge, "abandoned").await;
        bridge.park_driver("abandoned", driver).await;
        assert_eq!(bridge.parked_drivers().await, 1);

        // Nothing is due yet.
        assert_eq!(bridge.reap_idle(std::time::Duration::from_secs(3600)).await, 0);
        assert_eq!(bridge.parked_drivers().await, 1);

        // Everything is due.
        assert_eq!(bridge.reap_idle(std::time::Duration::ZERO).await, 1);
        assert_eq!(bridge.parked_drivers().await, 0);
        assert!(
            bridge.get("abandoned").await.is_none(),
            "reaping the driver must drop the session too, or the map still grows"
        );
    }

    /// The deadline caps how long one process lives; this caps how many exist.
    /// A client opening turns faster than it finishes them would otherwise stay
    /// under the deadline the whole way up.
    #[tokio::test]
    async fn parking_past_the_cap_evicts_the_oldest() {
        let bridge = Bridge::new();
        for n in 0..=MAX_PARKED {
            let key = format!("conv-{n}");
            let driver = idle_driver(&bridge, &key).await;
            bridge.park_driver(&key, driver).await;
        }
        assert_eq!(
            bridge.parked_drivers().await,
            MAX_PARKED,
            "the cap holds no matter how many arrive"
        );
        assert!(
            bridge.take_driver("conv-0").await.is_none(),
            "the oldest is the one that goes"
        );
        assert!(
            bridge.take_driver(&format!("conv-{MAX_PARKED}")).await.is_some(),
            "the newest is the one someone is waiting on"
        );
    }

    /// Taking a driver back must not leave the timestamp behind, or the next
    /// park would inherit an age it never had and be reaped early.
    #[tokio::test]
    async fn taking_and_re_parking_a_driver_restarts_its_clock() {
        let bridge = Bridge::new();
        let driver = idle_driver(&bridge, "c").await;
        bridge.park_driver("c", driver).await;
        let driver = bridge.take_driver("c").await.expect("parked");
        assert_eq!(bridge.parked_drivers().await, 0);
        bridge.park_driver("c", driver).await;
        assert_eq!(
            bridge.reap_idle(std::time::Duration::from_secs(3600)).await,
            0,
            "the re-parked driver is young again"
        );
        bridge.close("c").await;
    }
}
