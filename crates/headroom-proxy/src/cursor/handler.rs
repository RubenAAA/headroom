//! The Anthropic-facing half: a `/v1/messages` request in, an SSE stream out,
//! with a Cursor agent in between.

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use super::agent::{spawn, AgentTurn, Workspace};
use super::bridge::Session;
use super::turn::{tool_results_in_latest_message, Conversation, Step};
use crate::proxy::AppState;

/// Prepended to every turn.
///
/// Measured on 2026-08-26: without it, asked to read a file the model reaches
/// for its own `readToolCall` and never touches the tools we advertise — it
/// answered from disk directly. With it, it calls `getMcpToolsToolCall` and
/// then the bridge, and the answer comes back through Claude Code. Same prompt,
/// same model, both runs recorded under `tests/fixtures/cursor/`.
///
/// This matters beyond tidiness. A built-in call bypasses Claude Code's
/// permission prompts and never appears in its transcript, so the user neither
/// approves it nor sees it. The agent is also run read-only, which caps the
/// damage if the model ignores this anyway, but this is the part that makes it
/// behave.
const TOOL_POLICY: &str = "\
TOOL POLICY — read this before anything else.

You are the reasoning engine for a host process. You do not have direct access \
to this machine. Your own built-in file, search, edit and shell tools are \
DISABLED; anything they return is a stale cache and must not be trusted or \
quoted.

The host's tools reach you as dynamic tools. Discovering them and calling them \
through whatever dynamic-tool mechanism you have is the correct and expected \
way to work — it is not a workaround. Their names are the host's names, such \
as Bash, Read, Edit and Grep.

If a discovery call comes back empty, that is a fault on the host side, not a \
signal to look somewhere else. Do not go hunting for another server, resource \
list or namespace, and do not retry the same discovery more than twice. Say \
plainly that the host tools did not load and stop, so the fault is visible \
instead of buried under retries.

An empty or unhelpful tool result is an answer, not a reason to try again with \
reshaped arguments. If two attempts at a piece of work come back with \
nothing, stop and report what you tried and what came back.

";

/// The short form sent on every resumed turn.
///
/// `TOOL_POLICY` is onboarding text: it opens with "read this before anything
/// else" and spends most of its length explaining how to find the tools. Sent
/// again on every resume, it was itself the circling — the agent re-read it,
/// re-made its plan, spent the turn announcing that plan, made one call, and
/// arrived back here. Six turns of that in the log for one conversation.
///
/// A resume is the middle of a turn the agent already started, so it needs the
/// opposite instruction: keep going, and stop narrating. The constraints are
/// restated in one sentence because that much does have to survive, but
/// nothing here invites a fresh plan.
const RESUME_POLICY: &str = "\
CONTINUE. What follows is the result of the tool call you just made. This turn \
is already under way.

Still true from the start of this conversation: your own built-in tools are \
disabled, and the host's tools are the dynamic ones.

Do not re-plan. Do not restate your plan. Do not announce what you are about \
to do. Act on the result below, and when the work is finished say what you \
did.

";

/// Handle one `/v1/messages` request against a Cursor model.
pub(crate) async fn handle(
    state: AppState,
    parsed: &Value,
    session_key: &str,
    cursor_model: &str,
) -> Response {
    let key = crate::ctx::identity::conversation_key(parsed, session_key);

    // A turn carrying tool results is the continuation of a conversation that
    // is parked mid-tool: its agent is alive and blocked inside an MCP request.
    // Answer the parked calls and pick the same process back up.
    let results = tool_results_in_latest_message(parsed);
    if !results.is_empty() {
        if let (Some(session), Some(driver)) = (
            state.cursor_bridge.get(&key).await,
            state.cursor_bridge.take_driver(&key).await,
        ) {
            let mut delivered = 0usize;
            for (id, outcome) in &results {
                if session.answer(id, outcome.clone()).await {
                    delivered += 1;
                }
            }
            if delivered == 0 {
                // Every result was for a call this process never parked, which
                // is what a transcript replayed across a restart looks like.
                // Put the driver back and start fresh below.
                state.cursor_bridge.park_driver(&key, driver).await;
            } else {
                let mut driver = driver;
                driver.begin_response();
                tracing::debug!(
                    event = "cursor_turn_resumed",
                    conversation = %key,
                    delivered,
                    "released parked tool calls"
                );
                if let Some(tools) = parsed.get("tools").and_then(Value::as_array) {
                    session.set_tools(tools.clone()).await;
                }
                return drive(state.clone(), key, driver, wants_stream(parsed)).await;
            }
        }
    }

    let (session, inbox) = state.cursor_bridge.open(&key).await;
    let mut tool_count = 0usize;
    if let Some(tools) = parsed.get("tools").and_then(Value::as_array) {
        tool_count = tools.len();
        session.set_tools(tools.clone()).await;
    }
    tracing::info!(
        event = "cursor_turn_started",
        conversation = %key,
        model = %cursor_model,
        tools = tool_count,
        "starting a cursor turn"
    );

    let port = state.config.listen.port();
    let mcp_url = format!("http://127.0.0.1:{port}/mcp/{key}");
    let workspace = match Workspace::create(Some(&mcp_url)) {
        Ok(workspace) => workspace,
        Err(e) => {
            tracing::warn!(event = "cursor_workspace_failed", cause = ?e, "could not prepare a workspace");
            state.cursor_bridge.close(&key).await;
            return error_response(&format!("could not prepare a cursor workspace: {e}"));
        }
    };

    let turn = AgentTurn {
        model: cursor_model.to_string(),
        workspace: workspace.path().to_path_buf(),
        resume: session.chat_id().await,
        prompt: build_prompt(parsed, session.as_ref()).await,
        mcp_url: Some(mcp_url.clone()),
        timeout: None,
        // The workspace is an empty scratch directory, so the agent's own
        // file tools see nothing and every real read or write goes to the host
        // over the bridge, where the user approves it. The one way out of the
        // scratch directory is the agent's shell, so that gets sandboxed. What
        // this must not do is restrict the *mode*: `--mode ask` was the reason
        // the agent answered with a plan every turn instead of doing the work.
        sandbox: true,
    };

    let running = match spawn(&state.config.cursor_agent_binary, &turn).await {
        Ok(running) => running,
        Err(e) => {
            tracing::warn!(event = "cursor_agent_spawn_failed", cause = ?e, "could not start cursor-agent");
            state.cursor_bridge.close(&key).await;
            return error_response(&format!("cursor-agent failed to start: {e}"));
        }
    };

    let convo = Conversation::new(session, inbox, running, workspace);
    drive(state, key, convo, wants_stream(parsed)).await
}

/// Stream one response off a driver, and decide what becomes of the driver when
/// the response ends.
///
/// Three ways out, and they differ only in what happens to the agent process:
/// a pause parks it for the next request, an end reaps it, and a client that
/// hangs up reaps it too — otherwise it would sit blocked on a tool call that
/// nobody is ever going to answer.
/// Anthropic defaults `stream` to false, and a client that left it off wants
/// one JSON object back. Mirrors `handlers::local_model`, which has always
/// separated what it reads from the upstream from what it writes to the
/// client; this path used to stream at everyone regardless.
fn wants_stream(parsed: &Value) -> bool {
    parsed
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

async fn drive(state: AppState, key: String, convo: Conversation, stream: bool) -> Response {
    if stream {
        stream_driver(state, key, convo)
    } else {
        collect_driver(state, key, convo).await
    }
}

/// Run the turn to its end and answer with the single message it adds up to.
///
/// The agent is driven exactly as [`stream_driver`] drives it — same frames,
/// same parking on a tool call — and the frames are folded back into a message
/// here rather than written to the wire. Driving it differently would mean two
/// transports to keep honest.
async fn collect_driver(state: AppState, key: String, convo: Conversation) -> Response {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(16);
    spawn_driver(state, key, convo, tx);

    let mut frames = Vec::new();
    while let Some(frame) = rx.recv().await {
        match frame {
            Ok(f) => frames.push(f),
            Err(e) => return error_response(&format!("cursor agent failed: {e}")),
        }
    }

    match message_from_frames(&frames) {
        Some(message) => Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Body::from(message.to_string()))
            .unwrap_or_else(|_| error_response("could not build the response")),
        None => error_response("the cursor agent produced no message"),
    }
}

/// Fold the SSE frames of one turn back into an Anthropic message.
///
/// The deltas are the only place the content exists — the agent streams and
/// nothing upstream keeps a whole copy — so this reassembles rather than reads
/// a field. Unknown event types are skipped: a new one should not empty the
/// response.
fn message_from_frames(frames: &[String]) -> Option<Value> {
    let mut message: Option<Value> = None;
    let mut blocks: Vec<Value> = Vec::new();
    // `input_json_delta` arrives as text and is only valid JSON once complete,
    // so tool arguments are buffered per block and parsed at the end.
    let mut json_buf: std::collections::HashMap<usize, String> = std::collections::HashMap::new();

    for frame in frames {
        for line in frame.lines() {
            let Some(raw) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(raw) else {
                continue;
            };
            let idx = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            match event.get("type").and_then(|v| v.as_str()) {
                Some("message_start") => message = event.get("message").cloned(),
                Some("content_block_start") => {
                    if let Some(block) = event.get("content_block").cloned() {
                        while blocks.len() <= idx {
                            blocks.push(Value::Null);
                        }
                        blocks[idx] = block;
                    }
                }
                Some("content_block_delta") => {
                    let Some(delta) = event.get("delta") else {
                        continue;
                    };
                    let Some(block) = blocks.get_mut(idx) else {
                        continue;
                    };
                    match delta.get("type").and_then(|v| v.as_str()) {
                        Some("text_delta") => append_str(block, "text", delta.get("text")),
                        Some("thinking_delta") => {
                            append_str(block, "thinking", delta.get("thinking"))
                        }
                        Some("signature_delta") => {
                            append_str(block, "signature", delta.get("signature"))
                        }
                        Some("input_json_delta") => {
                            if let Some(part) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                json_buf.entry(idx).or_default().push_str(part);
                            }
                        }
                        _ => {}
                    }
                }
                Some("message_delta") => {
                    let msg = message.as_mut()?;
                    if let Some(delta) = event.get("delta").and_then(|v| v.as_object()) {
                        for (k, v) in delta {
                            msg[k.as_str()] = v.clone();
                        }
                    }
                    if let Some(usage) = event.get("usage") {
                        msg["usage"] = usage.clone();
                    }
                }
                _ => {}
            }
        }
    }

    for (idx, raw) in json_buf {
        if let Some(block) = blocks.get_mut(idx) {
            // An empty-argument tool call streams no delta at all, and the
            // `{}` the start frame carries is already right.
            if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                block["input"] = parsed;
            }
        }
    }

    let mut message = message?;
    message["content"] = Value::Array(blocks.into_iter().filter(|b| !b.is_null()).collect());
    Some(message)
}

fn append_str(block: &mut Value, field: &str, add: Option<&Value>) {
    let Some(add) = add.and_then(|v| v.as_str()) else {
        return;
    };
    let slot = block
        .get(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    block[field] = Value::String(slot + add);
}

/// Drive one turn, writing its SSE frames into `tx`.
///
/// Both transports use this, so a streamed turn and a collected one park,
/// shut down and reap the agent the same way.
///
/// Three ways out, and they differ only in what happens to the agent process:
/// a pause parks it for the next request, an end reaps it, and a receiver that
/// has gone away reaps it too — otherwise it would sit blocked on a tool call
/// that nobody is ever going to answer.
fn spawn_driver(
    state: AppState,
    key: String,
    mut convo: Conversation,
    tx: tokio::sync::mpsc::Sender<Result<String, std::io::Error>>,
) {
    let bridge = state.cursor_bridge.clone();
    tokio::spawn(async move {
        loop {
            match convo.next_step().await {
                Step::Emit(frames) => {
                    for frame in frames {
                        if tx.send(Ok(frame)).await.is_err() {
                            convo.shutdown().await;
                            bridge.close(&key).await;
                            return;
                        }
                    }
                }
                Step::Pause(frames) => {
                    for frame in frames {
                        let _ = tx.send(Ok(frame)).await;
                    }
                    // The response ends here; the conversation does not. The
                    // agent stays alive, blocked inside its MCP call, until a
                    // request arrives carrying the tool result.
                    bridge.park_driver(&key, convo).await;
                    return;
                }
                Step::End => {
                    convo.shutdown().await;
                    bridge.close(&key).await;
                    return;
                }
            }
        }
    });
}

fn stream_driver(state: AppState, key: String, convo: Conversation) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(16);
    spawn_driver(state, key, convo, tx);

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| error_response("could not build the response"))
}

/// Flatten the Anthropic transcript into the single prompt the CLI takes.
///
/// Only the newest user turn is sent when the conversation is being resumed:
/// Cursor keeps the history on its side, and re-sending it would both double
/// the bill and confuse a model that can already see it.
async fn build_prompt(parsed: &Value, session: &Session) -> String {
    let resuming = session.chat_id().await.is_some();
    let mut out = String::new();
    // Every turn carries a policy — sending it once meant a conversation
    // already going in circles could never be corrected — but not the same
    // one. Turn one is being onboarded; a resume is mid-turn and needs to be
    // told to carry on rather than to read the rules again. See
    // `RESUME_POLICY` for what re-sending the long form actually did.
    out.push_str(if resuming { RESUME_POLICY } else { TOOL_POLICY });
    if !resuming {
        if let Some(system) = system_text(parsed) {
            out.push_str("HOST INSTRUCTIONS\n\n");
            out.push_str(&system);
            out.push_str("\n\n");
        }
    }
    out.push_str(&transcript(parsed, resuming));
    out
}

fn system_text(parsed: &Value) -> Option<String> {
    match parsed.get("system")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        _ => None,
    }
}

fn transcript(parsed: &Value, latest_only: bool) -> String {
    let Some(messages) = parsed.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    let slice: &[Value] = if latest_only {
        messages.last().map(std::slice::from_ref).unwrap_or_default()
    } else {
        messages
    };
    slice
        .iter()
        .filter_map(|m| {
            let role = m.get("role").and_then(Value::as_str)?;
            let text = message_text(m);
            if text.trim().is_empty() {
                return None;
            }
            Some(format!("[{role}]: {text}"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(str::to_string),
                Some("tool_result") => b
                    .get("content")
                    .map(|c| format!("[tool result] {}", flatten(c))),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn flatten(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

fn error_response(message: &str) -> Response {
    (
        axum::http::StatusCode::BAD_GATEWAY,
        axum::Json(serde_json::json!({
            "type": "error",
            "error": {"type": "api_error", "message": message},
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn prompt_for(parsed: &Value, chat_id: Option<&str>) -> String {
        let bridge = crate::cursor::bridge::Bridge::new();
        let (session, _inbox) = bridge.open("k").await;
        if let Some(id) = chat_id {
            session.set_chat_id(id.to_string()).await;
        }
        build_prompt(parsed, session.as_ref()).await
    }

    /// The policy is what stops the model answering from its own file tools.
    /// It is the difference between a read the user approved and one they never
    /// saw, so a turn must not go out without it.
    #[tokio::test]
    async fn a_first_turn_carries_the_tool_policy_and_the_system_prompt() {
        let body = json!({
            "system": "You are working in the headroom repo.",
            "messages": [{"role": "user", "content": "what changed?"}],
        });
        let prompt = prompt_for(&body, None).await;
        assert!(prompt.starts_with("TOOL POLICY"));
        assert!(prompt.contains("built-in file, search, edit and shell tools are DISABLED"));
        assert!(prompt.contains("You are working in the headroom repo."));
        assert!(prompt.contains("[user]: what changed?"));
    }

    /// Cursor keeps the history server-side. Re-sending it would pay for the
    /// same tokens twice and show the model its own past twice over.
    #[tokio::test]
    async fn a_resumed_turn_sends_only_the_newest_message() {
        let body = json!({
            "system": "sys",
            "messages": [
                {"role": "user", "content": "first question"},
                {"role": "assistant", "content": "first answer"},
                {"role": "user", "content": "second question"},
            ],
        });
        let prompt = prompt_for(&body, Some("chat-1")).await;
        assert!(prompt.contains("second question"));
        assert!(!prompt.contains("first question"), "history is Cursor's job");
        assert!(
            !prompt.contains("HOST INSTRUCTIONS"),
            "the system prompt went with turn one"
        );
        // A policy still goes out — sending one only on turn one meant a
        // conversation already ignoring it could never be corrected — but the
        // short form, which says carry on rather than start over.
        assert!(
            prompt.starts_with("CONTINUE"),
            "a resume needs the short form: {prompt}"
        );
        assert!(
            !prompt.contains("TOOL POLICY"),
            "re-sending the onboarding text is what made it re-plan every turn"
        );
    }

    #[tokio::test]
    async fn a_block_shaped_system_prompt_is_joined() {
        let body = json!({
            "system": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}],
            "messages": [{"role": "user", "content": "q"}],
        });
        let prompt = prompt_for(&body, None).await;
        assert!(prompt.contains("one\n\ntwo"));
    }

    #[tokio::test]
    async fn tool_results_are_rendered_so_the_model_can_read_them() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "file contents here"}
        ]}]});
        let prompt = prompt_for(&body, Some("chat-1")).await;
        assert!(prompt.contains("[tool result] file contents here"));
    }

    #[tokio::test]
    async fn an_empty_transcript_does_not_panic() {
        assert!(
            !prompt_for(&json!({}), None).await.is_empty(),
            "the policy still goes out"
        );
        // A resumed turn with nothing new still carries a policy, which is
        // the whole point of repeating one.
        let resumed = prompt_for(&json!({"messages": []}), Some("c")).await;
        assert!(resumed.starts_with("CONTINUE"));
        assert!(!resumed.contains("[user]"), "and no transcript beyond it");
    }
}

#[cfg(test)]
mod non_streaming_tests {
    use super::*;
    use serde_json::json;

    fn frame(event: &str, data: Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    #[test]
    fn stream_defaults_to_false_the_way_anthropic_does() {
        assert!(!wants_stream(&json!({"model": "m"})));
        assert!(!wants_stream(&json!({"stream": false})));
        assert!(wants_stream(&json!({"stream": true})));
    }

    /// Text and thinking arrive only as deltas, so the fold has to concatenate
    /// them; reading a field would return the empty string the start frame
    /// carries.
    #[test]
    fn text_and_thinking_deltas_are_concatenated() {
        let frames = vec![
            frame("message_start", json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant",
                "model": "cursor-grok-4.6-high", "content": [], "stop_reason": null}})),
            frame("content_block_start", json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}})),
            frame("content_block_delta", json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "let me "}})),
            frame("content_block_delta", json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "count"}})),
            frame("content_block_start", json!({"type": "content_block_start", "index": 1,
                "content_block": {"type": "text", "text": ""}})),
            frame("content_block_delta", json!({"type": "content_block_delta", "index": 1,
                "delta": {"type": "text_delta", "text": "39"}})),
            frame("content_block_delta", json!({"type": "content_block_delta", "index": 1,
                "delta": {"type": "text_delta", "text": "1"}})),
            frame("message_delta", json!({"type": "message_delta",
                "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 7}})),
            frame("message_stop", json!({"type": "message_stop"})),
        ];

        let msg = message_from_frames(&frames).expect("a message");
        assert_eq!(msg["id"], "msg_1");
        assert_eq!(msg["content"][0]["thinking"], "let me count");
        assert_eq!(msg["content"][1]["text"], "391");
        assert_eq!(msg["stop_reason"], "end_turn");
        assert_eq!(msg["usage"]["output_tokens"], 7);
    }

    /// Tool arguments stream as JSON text that is only parseable once whole.
    /// Applying each fragment as it lands would leave `input` a string.
    #[test]
    fn tool_input_is_parsed_from_the_whole_json_not_the_fragments() {
        let frames = vec![
            frame("message_start", json!({"type": "message_start",
                "message": {"id": "msg_2", "content": []}})),
            frame("content_block_start", json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {}}})),
            frame("content_block_delta", json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"city\":"}})),
            frame("content_block_delta", json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "\"Yerevan\"}"}})),
            frame("message_delta", json!({"type": "message_delta",
                "delta": {"stop_reason": "tool_use"}})),
        ];

        let msg = message_from_frames(&frames).expect("a message");
        assert_eq!(msg["content"][0]["type"], "tool_use");
        assert_eq!(msg["content"][0]["input"], json!({"city": "Yerevan"}));
        assert_eq!(msg["stop_reason"], "tool_use");
    }

    /// A tool taking no arguments streams no delta. The `{}` from the start
    /// frame is already right and must survive.
    #[test]
    fn a_tool_call_with_no_arguments_keeps_its_empty_input() {
        let frames = vec![
            frame("message_start", json!({"type": "message_start", "message": {"content": []}})),
            frame("content_block_start", json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": "t", "name": "now", "input": {}}})),
        ];
        let msg = message_from_frames(&frames).expect("a message");
        assert_eq!(msg["content"][0]["input"], json!({}));
    }

    /// A turn that produced nothing must not be answered with a half-built
    /// object the client would have to guess about.
    #[test]
    fn no_message_start_means_no_message() {
        assert!(message_from_frames(&[]).is_none());
        assert!(message_from_frames(&[frame("ping", json!({"type": "ping"}))]).is_none());
    }

    /// An event type nobody has taught this about should cost the response
    /// nothing.
    #[test]
    fn an_unknown_event_is_skipped_rather_than_fatal() {
        let frames = vec![
            frame("message_start", json!({"type": "message_start", "message": {"content": []}})),
            frame("something_new", json!({"type": "something_new", "index": 0, "wat": true})),
            frame("content_block_start", json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": "ok"}})),
        ];
        let msg = message_from_frames(&frames).expect("a message");
        assert_eq!(msg["content"][0]["text"], "ok");
    }
}
