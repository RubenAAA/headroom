//! Driving one conversation: the agent process, the parked tools, and where a
//! response ends.
//!
//! A conversation outlives the HTTP request that started it. Claude Code sends
//! a request, the agent runs, and if the model reaches for a tool the response
//! ends early with `stop_reason: "tool_use"` — but the agent process does not.
//! It stays alive, blocked inside its MCP call, until the next request brings
//! the `tool_result` back. So the process lives in the `Conversation`, and
//! requests attach to it and detach.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use super::agent::{RunningTurn, Workspace};
use super::bridge::{ParkedCall, Session};

/// What the driver wants the response writer to do next.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Write these frames and keep going.
    Emit(Vec<String>),
    /// Write these frames and end the response. The conversation stays open.
    Pause(Vec<String>),
    /// The turn is over. Nothing more to write.
    End,
}

pub(crate) struct Conversation {
    pub(crate) session: Arc<Session>,
    inbox: UnboundedReceiver<ParkedCall>,
    running: RunningTurn,
    /// Held, not used. The agent reads its `.cursor/mcp.json` out of here, and
    /// dropping it deletes the directory — so it has to live exactly as long as
    /// the process does, which is longer than the request that started it.
    _workspace: Workspace,
    /// Set once `system/init` names the chat, so the next turn can `--resume`.
    chat_id_recorded: bool,
}

impl Conversation {
    pub(crate) fn new(
        session: Arc<Session>,
        inbox: UnboundedReceiver<ParkedCall>,
        running: RunningTurn,
        workspace: Workspace,
    ) -> Self {
        Self {
            session,
            inbox,
            running,
            _workspace: workspace,
            chat_id_recorded: false,
        }
    }

    /// Wait for whichever comes first: more output from the agent, or a tool
    /// call parked by the MCP endpoint.
    ///
    /// The race is the point. Both arrive on the agent's behalf but by
    /// different routes — output down its stdout, tool calls in over HTTP —
    /// and either can be next.
    pub(crate) async fn next_step(&mut self) -> Step {
        loop {
            tokio::select! {
                // Biased so a parked call is taken before more output when both
                // are ready. Cursor writes its narration before it blocks on the
                // call; taking the output first would be right too, but fixing
                // the order keeps the frame sequence reproducible in tests.
                biased;

                parked = self.inbox.recv() => {
                    let Some(parked) = parked else {
                        // The session was closed underneath us.
                        return Step::End;
                    };
                    let mut frames = self.running.translator.emit_parked_tool_use(
                        &parked.id,
                        &parked.name,
                        &parked.args,
                    );
                    frames.extend(self.running.translator.pause_for_tool());
                    return Step::Pause(frames);
                }

                frames = self.running.next_frames() => {
                    match frames {
                        Some(frames) => {
                            self.record_chat_id().await;
                            return Step::Emit(frames);
                        }
                        None => return Step::End,
                    }
                }
            }
        }
    }

    async fn record_chat_id(&mut self) {
        if self.chat_id_recorded {
            return;
        }
        if let Some(id) = self.running.translator.session_id.clone() {
            self.session.set_chat_id(id).await;
            self.chat_id_recorded = true;
        }
    }

    /// Kill the agent and reap it.
    pub(crate) async fn shutdown(&mut self) {
        self.running.finish().await;
    }
}

/// The `tool_result` blocks in the newest user message, as (id, outcome).
///
/// Read from the tail rather than the whole transcript: Claude Code resends the
/// entire history every turn, and answering an id from ten turns ago would push
/// a stale result into a call parked now. Only the newest message can hold the
/// answer to the call this conversation is actually blocked on.
pub(crate) fn tool_results_in_latest_message(body: &Value) -> Vec<(String, super::bridge::ToolOutcome)> {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return Vec::new();
    };
    let Some(last) = messages.last() else {
        return Vec::new();
    };
    let Some(blocks) = last.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| {
            let id = b.get("tool_use_id").and_then(Value::as_str)?.to_string();
            let text = tool_result_text(b);
            let failed = b.get("is_error").and_then(Value::as_bool) == Some(true);
            Some((
                id,
                if failed {
                    super::bridge::ToolOutcome::Failed(text)
                } else {
                    super::bridge::ToolOutcome::Ok(text)
                },
            ))
        })
        .collect()
}

/// The text of a `tool_result`, which Claude Code writes either as a bare
/// string or as a list of blocks depending on the tool.
fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_string_tool_result_is_read() {
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "file body"}]}
        ]});
        let got = tool_results_in_latest_message(&body);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "toolu_1");
        assert_eq!(got[0].1, super::super::bridge::ToolOutcome::Ok("file body".into()));
    }

    #[test]
    fn a_block_shaped_tool_result_is_flattened() {
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_2",
              "content": [{"type": "text", "text": "line one"}, {"type": "text", "text": "line two"}]}]}
        ]});
        let got = tool_results_in_latest_message(&body);
        assert_eq!(got[0].1, super::super::bridge::ToolOutcome::Ok("line one\nline two".into()));
    }

    #[test]
    fn an_errored_tool_result_keeps_its_error_flag() {
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "boom", "is_error": true}]}
        ]});
        assert_eq!(
            tool_results_in_latest_message(&body)[0].1,
            super::super::bridge::ToolOutcome::Failed("boom".into())
        );
    }

    /// Claude Code resends the whole history every turn. Reading results from
    /// anywhere but the newest message would answer a call parked now with an
    /// outcome from ten turns ago.
    #[test]
    fn only_the_newest_message_is_read() {
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "old", "content": "stale"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "…"}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "new", "content": "fresh"}]}
        ]});
        let got = tool_results_in_latest_message(&body);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "new");
    }

    #[test]
    fn a_turn_with_no_tool_results_yields_none() {
        let body = json!({"messages": [{"role": "user", "content": "just a question"}]});
        assert!(tool_results_in_latest_message(&body).is_empty());
        assert!(tool_results_in_latest_message(&json!({})).is_empty());
    }

    #[test]
    fn several_results_in_one_message_all_come_back() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "a", "content": "1"},
            {"type": "text", "text": "and here is more"},
            {"type": "tool_result", "tool_use_id": "b", "content": "2"}
        ]}]});
        let got = tool_results_in_latest_message(&body);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "a");
        assert_eq!(got[1].0, "b");
    }

    use super::super::agent::{spawn, AgentTurn};
    use super::super::bridge::{handle_rpc, Bridge, ToolOutcome};

    /// A stand-in for `cursor-agent` that speaks, then blocks the way the real
    /// one blocks on an MCP call, then finishes once released.
    fn stub_agent(dir: &std::path::Path, gate: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("stub-agent");
        let gate = gate.display();
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n\
                 cat > /dev/null\n\
                 echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-stub\"}}'\n\
                 echo '{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"let me look\"}}]}}}}'\n\
                 while [ ! -f {gate} ]; do sleep 0.02; done\n\
                 echo '{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"the marker is CRIMSON-42\"}}]}}}}'\n\
                 echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"usage\":{{\"inputTokens\":11,\"outputTokens\":22,\"cacheReadTokens\":0,\"cacheWriteTokens\":0}}}}'\n"
            ),
        )
        .expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path
    }

    fn text_of(frames: &[String]) -> String {
        frames.concat()
    }

    /// The whole mechanism, end to end over a real pipe and a real process: the
    /// agent speaks, reaches for a tool, the response ends with
    /// `stop_reason: "tool_use"` while the process stays alive, the result is
    /// delivered, and the same process finishes the answer.
    #[tokio::test]
    async fn a_parked_tool_pauses_the_response_and_the_agent_resumes_after_the_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gate = dir.path().join("released");
        let stub = stub_agent(dir.path(), &gate);

        let bridge = Bridge::new();
        let (session, inbox) = bridge.open("conv-1").await;
        session
            .set_tools(vec![json!({
                "name": "Read",
                "description": "read a file",
                "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}},
            })])
            .await;

        let running = spawn(
            stub.to_str().unwrap(),
            &AgentTurn {
                model: "cursor-grok-4.6-high".into(),
                workspace: dir.path().to_path_buf(),
                resume: None,
                prompt: "what is the marker?".into(),
                mcp_url: Some("http://127.0.0.1:0/mcp/conv-1".into()),
                timeout: None,
                read_only: false,
            },
        )
        .await
        .expect("spawn stub");

        let mut convo = Conversation::new(session.clone(), inbox, running, Workspace::create(None).expect("workspace"));

        // The agent talks before it reaches for anything.
        let mut before = String::new();
        loop {
            match convo.next_step().await {
                Step::Emit(frames) => {
                    before.push_str(&text_of(&frames));
                    if before.contains("let me look") {
                        break;
                    }
                }
                other => panic!("expected output first, got {other:?}"),
            }
        }
        assert!(before.starts_with("event: message_start\n"));
        assert_eq!(
            session.chat_id().await.as_deref(),
            Some("sess-stub"),
            "the chat id is recorded as soon as it is seen, so the next turn can resume"
        );

        // Now the model calls a tool. In the real system this arrives over HTTP
        // from the agent process; here it is the same code path, called directly.
        let calling = {
            let session = session.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                let reply = handle_rpc(
                    &session,
                    &json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                            "params": {"name": "Read", "arguments": {"file_path": "/tmp/marker.txt"}}}),
                )
                .await;
                // Releasing the agent only once the call has been answered is
                // what the real agent does: it is blocked on this response.
                std::fs::write(&gate, "go").expect("release");
                reply
            })
        };

        // The response ends here, with the tool handed up to the caller.
        let paused = match convo.next_step().await {
            Step::Pause(frames) => text_of(&frames),
            other => panic!("expected a pause, got {other:?}"),
        };
        assert!(paused.contains(r#""type":"tool_use""#));
        assert!(paused.contains(r#""name":"Read""#));
        assert!(paused.contains(r#"/tmp/marker.txt"#), "the arguments came through");
        assert!(paused.contains(r#""stop_reason":"tool_use""#));
        assert!(paused.trim_end().ends_with(r#""type":"message_stop"}"#));
        assert!(session.has_parked_calls().await, "the agent is still waiting");

        // Claude Code runs the tool and comes back with the answer.
        let parked_id = paused
            .split(r#""id":"toolu_"#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .map(|n| format!("toolu_{n}"))
            .expect("a tool_use id");
        assert!(
            session.answer(&parked_id, ToolOutcome::Ok("DISK: CRIMSON-42".into())).await,
            "the id in the tool_use block is the id the bridge parked under"
        );

        let mcp_reply = calling.await.expect("join").expect("answered");
        assert_eq!(mcp_reply["result"]["content"][0]["text"], "DISK: CRIMSON-42");

        // The same process picks up where it left off.
        let mut after = String::new();
        loop {
            match convo.next_step().await {
                Step::Emit(frames) => after.push_str(&text_of(&frames)),
                Step::End => break,
                Step::Pause(_) => panic!("nothing else should park"),
            }
        }
        assert!(after.contains("CRIMSON-42"), "got: {after}");
        assert!(after.contains(r#""stop_reason":"end_turn""#));
        assert!(after.contains(r#""input_tokens":11"#), "usage survived the pause");
        assert!(!session.has_parked_calls().await);

        convo.shutdown().await;
    }

    /// Closing the session while the agent runs must end the turn rather than
    /// leave the driver waiting on a channel nobody holds.
    #[tokio::test]
    async fn a_closed_session_ends_the_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gate = dir.path().join("never");
        let stub = stub_agent(dir.path(), &gate);
        let bridge = Bridge::new();
        let (session, inbox) = bridge.open("conv-2").await;
        let running = spawn(
            stub.to_str().unwrap(),
            &AgentTurn {
                model: "m".into(),
                workspace: dir.path().to_path_buf(),
                resume: None,
                prompt: "hi".into(),
                mcp_url: None,
                timeout: None,
                read_only: false,
            },
        )
        .await
        .expect("spawn");
        let mut convo = Conversation::new(session, inbox, running, Workspace::create(None).expect("workspace"));
        // Drain the two lines the stub writes before it blocks.
        assert!(matches!(convo.next_step().await, Step::Emit(_)));
        assert!(matches!(convo.next_step().await, Step::Emit(_)));
        // Drop every sender, which is what closing the session does.
        bridge.close("conv-2").await;
        convo.shutdown().await;
    }
}
