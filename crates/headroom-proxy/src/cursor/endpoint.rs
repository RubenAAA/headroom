//! The MCP endpoint Cursor's agent talks to.
//!
//! One path per conversation. The conversation key is in the URL rather than in
//! a header because that is all `.cursor/mcp.json` lets us set — the file takes
//! a bare `url` and the agent adds nothing of its own to identify itself.
//!
//! Bound to loopback only, in the same way the `/debug` routes are. The endpoint
//! hands out whatever tools the conversation advertised and blocks on them; it
//! is not something to expose.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::proxy::AppState;

/// `POST /mcp/{conversation}` — one JSON-RPC message.
///
/// A `tools/call` does not return here until Claude Code has run the tool,
/// which may be minutes. That is deliberate; see `super::bridge`.
pub async fn handle_mcp(
    State(state): State<AppState>,
    Path(conversation): Path<String>,
    Json(request): Json<Value>,
) -> Response {
    let Some(session) = state.cursor_bridge.get(&conversation).await else {
        // The turn that opened this conversation is gone. Say so in JSON-RPC
        // rather than with a bare 404: the agent is an MCP client and will
        // report a transport failure as a crash, but an error it can read as
        // a tool failure it will simply tell the model about.
        return Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "error": {"code": -32000, "message": "this conversation is no longer open"},
        }))
        .into_response();
    };

    match super::bridge::handle_rpc(&session, &request).await {
        Some(reply) => Json(reply).into_response(),
        // A notification. MCP over HTTP wants 202 and an empty body.
        None => StatusCode::ACCEPTED.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::bridge::ToolOutcome;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        AppState::new(crate::config::Config::for_test(
            "http://127.0.0.1:9".parse().expect("url"),
        ))
        .expect("app state")
    }

    fn router(state: AppState) -> axum::Router {
        axum::Router::new()
            .route("/mcp/{conversation}", axum::routing::post(handle_mcp))
            .with_state(state)
    }

    async fn post(app: axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn tools_list_over_http_returns_the_conversations_tools() {
        let state = test_state();
        let (session, _inbox) = state.cursor_bridge.open("conv-a").await;
        session
            .set_tools(vec![json!({
                "name": "Read",
                "description": "read a file",
                "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}},
            })])
            .await;

        let (status, body) = post(
            router(state),
            "/mcp/conv-a",
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["tools"][0]["name"], "Read");
        assert_eq!(body["result"]["tools"][0]["inputSchema"]["type"], "object");
    }

    /// Conversations must not see each other's tools. The key in the path is
    /// the only thing separating them.
    #[tokio::test]
    async fn conversations_are_isolated_by_the_key_in_the_path() {
        let state = test_state();
        let (a, _ia) = state.cursor_bridge.open("conv-a").await;
        let (b, _ib) = state.cursor_bridge.open("conv-b").await;
        a.set_tools(vec![json!({"name": "OnlyA", "input_schema": {}})]).await;
        b.set_tools(vec![json!({"name": "OnlyB", "input_schema": {}})]).await;

        let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let (_, from_a) = post(router(state.clone()), "/mcp/conv-a", list.clone()).await;
        let (_, from_b) = post(router(state), "/mcp/conv-b", list).await;
        assert_eq!(from_a["result"]["tools"][0]["name"], "OnlyA");
        assert_eq!(from_b["result"]["tools"][0]["name"], "OnlyB");
    }

    /// A dead conversation must come back as something the agent can report to
    /// the model, not as a transport failure it treats as a crash.
    #[tokio::test]
    async fn an_unknown_conversation_is_a_readable_jsonrpc_error() {
        let state = test_state();
        let (status, body) = post(
            router(state),
            "/mcp/never-opened",
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "not a 404: the agent must be able to read it");
        assert_eq!(body["error"]["code"], -32000);
        assert_eq!(body["id"], 4);
    }

    #[tokio::test]
    async fn a_notification_gets_202_and_no_body() {
        let state = test_state();
        let (_s, _i) = state.cursor_bridge.open("conv-a").await;
        let (status, _) = post(
            router(state),
            "/mcp/conv-a",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    /// The endpoint holds the HTTP request open for as long as the tool takes.
    /// This is the property the whole design rests on, so it is checked over a
    /// real request rather than only at the bridge.
    #[tokio::test]
    async fn a_tool_call_holds_the_http_request_open_until_it_is_answered() {
        let state = test_state();
        let (session, mut inbox) = state.cursor_bridge.open("conv-a").await;
        session.set_tools(vec![json!({"name": "Read", "input_schema": {}})]).await;

        let request = tokio::spawn(post(
            router(state),
            "/mcp/conv-a",
            json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                   "params": {"name": "Read", "arguments": {"file_path": "/tmp/x"}}}),
        ));

        let parked = inbox.recv().await.expect("the call reaches the turn loop");
        assert!(!request.is_finished(), "the request must still be open");

        session.answer(&parked.id, ToolOutcome::Ok("contents".into())).await;
        let (status, body) = request.await.expect("join");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["content"][0]["text"], "contents");
    }
}
