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
to this machine. Your built-in file, search, edit and shell tools are DISABLED; \
anything they return is a stale cache and must not be trusted or quoted.

The ONLY working tools are the ones provided by the `headroom` MCP server. \
Every file read, every search, every command, every edit goes through them. If \
you need a tool and cannot see it, say so plainly instead of falling back to a \
built-in one.

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
                tracing::debug!(
                    event = "cursor_turn_resumed",
                    conversation = %key,
                    delivered,
                    "released parked tool calls"
                );
                if let Some(tools) = parsed.get("tools").and_then(Value::as_array) {
                    session.set_tools(tools.clone()).await;
                }
                return stream_driver(state.clone(), key, driver);
            }
        }
    }

    let (session, inbox) = state.cursor_bridge.open(&key).await;
    if let Some(tools) = parsed.get("tools").and_then(Value::as_array) {
        session.set_tools(tools.clone()).await;
    }

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
        // Built-in tools are capped at read-only. The tool policy is what
        // actually redirects the model; this is the backstop for when it does
        // not listen, and it is the difference between an unapproved read and
        // an unapproved write.
        read_only: true,
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
    stream_driver(state, key, convo)
}

/// Stream one response off a driver, and decide what becomes of the driver when
/// the response ends.
///
/// Three ways out, and they differ only in what happens to the agent process:
/// a pause parks it for the next request, an end reaps it, and a client that
/// hangs up reaps it too — otherwise it would sit blocked on a tool call that
/// nobody is ever going to answer.
fn stream_driver(state: AppState, key: String, mut convo: Conversation) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(16);
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
    if !resuming {
        out.push_str(TOOL_POLICY);
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
        assert!(!prompt.contains("TOOL POLICY"), "the policy was set on turn one");
        assert!(!prompt.contains("sys"), "the system prompt went with turn one");
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
        assert!(!prompt_for(&json!({}), None).await.is_empty(), "the policy still goes out");
        assert!(prompt_for(&json!({"messages": []}), Some("c")).await.is_empty());
    }
}
