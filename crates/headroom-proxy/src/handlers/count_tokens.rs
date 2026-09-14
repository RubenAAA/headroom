//! Local token counting for translated routes (`POST /v1/messages/count_tokens`).
//!
//! Claude Code calls this to estimate context. A routed alias
//! (`claude-muse-spark-1.3`) forwarded verbatim is unknown to Anthropic,
//! which answers 404 with a non-JSON page — and that refusal also pollutes
//! the upstream-health window the statusline watches. Answer translated
//! routes locally with the calibrated estimator instead.
//!
//! Anything else falls through to the standard forwarder byte-identical, so
//! counts for real Anthropic models stay exact and behavior there cannot
//! regress: this endpoint only changes responses that are 404s today.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::Value;

use crate::proxy::{forward_http, AppState};
use crate::routed::routing::find_route_target;

/// Estimate Anthropic input tokens for a `count_tokens` body: system text,
/// message content, and tool definitions, through the model family's
/// calibrated counter (`claude-*` → 3.5 chars/token estimator, tiktoken
/// where a real tokenizer exists).
///
/// Deliberately an estimate, and a slight undercount at that: per-message
/// framing is not counted. Image/document payloads ARE weighted (see
/// `image_block_tokens`): without that the estimate stays flat as screenshots
/// accumulate, the client never compacts, and the turn dies at the upstream
/// byte cap. The alternative for these models today is a 404, so an honest
/// approximation strictly wins.
pub(crate) fn estimate_input_tokens(parsed: &Value, model: &str) -> u64 {
    let counter = headroom_core::tokenizer::get_tokenizer(model);
    let mut text = String::new();
    let mut image_tokens: u64 = 0;
    push_content_text(&mut text, &mut image_tokens, &parsed["system"]);
    if let Some(messages) = parsed.get("messages").and_then(|m| m.as_array()) {
        for message in messages {
            push_content_text(&mut text, &mut image_tokens, &message["content"]);
        }
    }
    // Tool definitions ride every request as input tokens.
    if parsed.get("tools").is_some() {
        text.push_str(&serde_json::to_string(&parsed["tools"]).unwrap_or_default());
    }
    counter.count_text(&text) as u64 + image_tokens
}

/// Append the countable text of an Anthropic `content` value: a plain string,
/// or an array of blocks. Image/document blocks carry no text (pixels are
/// billed by size, which a text counter cannot see) — their weight goes to
/// `image_tokens` instead of being skipped.
fn push_content_text(out: &mut String, image_tokens: &mut u64, content: &Value) {
    match content {
        Value::String(s) => out.push_str(s),
        Value::Array(blocks) => {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(s) = block.get("text").and_then(|t| t.as_str()) {
                            out.push_str(s);
                        }
                    }
                    Some("tool_use") => {
                        // The JSON the model must reproduce counts.
                        out.push_str(&serde_json::to_string(&block["input"]).unwrap_or_default());
                    }
                    Some("tool_result") => push_content_text(out, image_tokens, &block["content"]),
                    Some("image") | Some("document") => {
                        *image_tokens += image_block_tokens(block);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Token weight of an image/document block, which carries no countable text.
///
/// Anthropic bills images by dimensions ((w*h)/750), not bytes, so base64
/// length alone cannot size them: read just the image header for dimensions
/// (no pixel decode) and apply the same formula as
/// `tile_optimizer::estimate_anthropic_tokens`. When dimensions are
/// unreadable (unsupported format, URL source, corrupt data) or the block is
/// a document (PDFs have no pixel dimensions but still ride the request as
/// base64), fall back to decoded bytes/750 — roughly one byte per pixel.
fn image_block_tokens(block: &Value) -> u64 {
    let source = match block.get("source") {
        Some(s) => s,
        None => return 0,
    };
    // URL sources live upstream; nothing local to weigh.
    if source.get("type").and_then(|t| t.as_str()) != Some("base64") {
        return 0;
    }
    let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
    // Tolerate a data: URL prefix though Anthropic sends raw base64.
    let b64 = data.rsplit(',').next().unwrap_or(data);
    let decoded_len = b64.len() as u64 * 3 / 4;
    let bytes = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64) {
        Ok(b) => b,
        Err(_) => return decoded_len / 750,
    };
    if block.get("type").and_then(|t| t.as_str()) != Some("image") {
        return bytes.len() as u64 / 750;
    }
    match image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
    {
        Some((w, h)) => crate::tile_optimizer::estimate_anthropic_tokens(w, h) as u64,
        None => bytes.len() as u64 / 750,
    }
}

/// Forward a request untouched through the standard pipeline. Shared by the
/// unparseable-body and unrouted-model exits below: both keep today's
/// behavior byte-identical.
async fn forward_unchanged(
    state: AppState,
    client_addr: SocketAddr,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if let Some(hs) = builder.headers_mut() {
        *hs = headers;
    }
    let req = builder.body(Body::from(body)).expect("valid request");
    forward_http(state, client_addr, req)
        .await
        .unwrap_or_else(|e| e.into_response())
}

/// Handle `POST /v1/messages/count_tokens`.
pub(crate) async fn handle_count_tokens(
    State(state): State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        // Not JSON — cannot name a routed model; forward byte-identical.
        Err(_) => {
            return forward_unchanged(state, client_addr, method, uri, headers, body).await;
        }
    };
    let model = parsed
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    // Cursor routes run a subprocess, not HTTP: no upstream could count
    // these, so answer locally like translated routes (forwarding would hit
    // the default Anthropic upstream with an unknown id: 404 plus the same
    // health-window pollution this endpoint exists to avoid).
    if let Some(agent) = state
        .config
        .model_routes
        .iter()
        .find(|r| r.matches(model))
        .and_then(|r| r.resolve_cursor_agent(crate::output_shaper::requested_effort(&parsed)))
    {
        let input_tokens = estimate_input_tokens(&parsed, &agent);
        tracing::info!(
            event = "count_tokens_local",
            model = %model,
            agent = %agent,
            input_tokens = input_tokens,
            "count_tokens for a cursor route answered locally"
        );
        return axum::Json(serde_json::json!({ "input_tokens": input_tokens })).into_response();
    }
    // Only translated routes are unservable upstream (cursor routes are
    // handled above): their target speaks another protocol, so no model
    // string we could put here would count.
    // Everything else (real Anthropic models, non-translating passthrough
    // routes) forwards byte-identical and keeps the exact upstream count.
    let translated = find_route_target(&state.config, model).is_some_and(|t| t.translate);
    if !translated {
        return forward_unchanged(state, client_addr, method, uri, headers, body).await;
    }
    let input_tokens = estimate_input_tokens(&parsed, model);
    tracing::info!(
        event = "count_tokens_local",
        model = %model,
        input_tokens = input_tokens,
        "count_tokens for a translated route answered locally"
    );
    axum::Json(serde_json::json!({ "input_tokens": input_tokens })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Value {
        serde_json::json!({
            "model": "claude-muse-spark-1.3",
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "hello world, this is a test message"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "hi there"},
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "/tmp/x"}},
                ]},
            ],
            "tools": [{"name": "Read", "description": "read a file"}],
        })
    }

    #[test]
    fn estimate_counts_system_messages_and_tools() {
        let base = estimate_input_tokens(&body(), "claude-muse-spark-1.3");
        assert!(base > 0, "a real body counts for something");

        let mut more = body();
        more["messages"].as_array_mut().unwrap().push(
            serde_json::json!({"role": "user", "content": "hello world, this is a test message"}),
        );
        assert!(
            estimate_input_tokens(&more, "claude-muse-spark-1.3") > base,
            "more text counts more"
        );

        let mut no_tools = body();
        no_tools.as_object_mut().unwrap().remove("tools");
        assert!(
            estimate_input_tokens(&no_tools, "claude-muse-spark-1.3") < base,
            "tool definitions ride along as input"
        );
    }

    #[test]
    fn estimate_weights_images_so_compaction_can_fire() {
        let mut with_image = body();
        // 1x1 PNG: decodes to real dimensions, weighs (w*h)/750 = 1 token min.
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        with_image["messages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "role": "user",
                "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png_b64}}],
            }));
        let base = estimate_input_tokens(&body(), "claude-muse-spark-1.3");
        assert!(
            estimate_input_tokens(&with_image, "claude-muse-spark-1.3") > base,
            "image bytes must move the count or the client never compacts"
        );
    }

    #[test]
    fn tool_result_text_counts_recursively() {
        let mut block = body();
        block["messages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1",
                             "content": [{"type": "text", "text": "file contents here"}]}],
            }));
        assert!(
            estimate_input_tokens(&block, "claude-muse-spark-1.3")
                > estimate_input_tokens(&body(), "claude-muse-spark-1.3")
        );
    }
}
