//! Admission gate: request rate, token rate and spend budget.
//!
//! One middleware in front of every generation route — Anthropic Messages
//! (plain and Foundry), OpenAI Chat Completions and Responses, and Gemini
//! `generateContent` — so no route can skip a limit. Upstream enforced these
//! per handler and missed routes more than once (`138736c9` found TPM never
//! checked, `f734c573` found the budget checked on Anthropic only). Here
//! `/v1/messages` usually reaches the catch-all rather than a handler, which
//! is another reason the gate sits in front of routing.
//!
//! Everything is off unless configured: an RPM or TPM of 0 is unlimited, and
//! without `--budget-limit-usd` the budget always allows. Internal
//! re-dispatches call `forward_http` directly, so they are never charged
//! twice.
//!
//! The Codex WebSocket is a GET and passes through here; it checks the budget
//! itself, before the upgrade and on every `response.create` frame.

use std::sync::OnceLock;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;

use crate::proxy::AppState;

/// Whether `path` makes a model generate (and so costs money and quota).
fn is_generation_path(path: &str) -> bool {
    crate::compression::is_compressible_path(path)
        || path == "/anthropic/v1/messages"
        || gemini_generate_model(path).is_some()
}

/// The model named by a Gemini `generateContent` / `streamGenerateContent`
/// path, e.g. `/v1beta/models/gemini-2.5-pro:generateContent`.
fn gemini_generate_model(path: &str) -> Option<&str> {
    let (model, action) = path.strip_prefix("/v1beta/models/")?.split_once(':')?;
    matches!(action, "generateContent" | "streamGenerateContent").then_some(model)
}

/// The bucket a caller's requests count against: an HMAC of its credential,
/// so bucket keys never hold a recoverable secret. `Authorization` first,
/// then Anthropic's `x-api-key`, then Azure-style `api-key` (upstream
/// `b9e8462a`: clients sending only `api-key` all shared one bucket). The
/// key is random per process, like the buckets themselves.
pub(crate) fn rate_limit_key(headers: &HeaderMap) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    static SECRET: OnceLock<[u8; 32]> = OnceLock::new();

    let credential = ["authorization", "x-api-key", "api-key"]
        .into_iter()
        .find_map(|name| {
            let value = headers.get(name)?.to_str().ok()?;
            (!value.is_empty()).then_some((name, value))
        });
    let Some((kind, value)) = credential else {
        return "default".to_string();
    };
    let secret = SECRET.get_or_init(|| {
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key
    });
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(secret).expect("HMAC takes a key of any length");
    mac.update(kind.as_bytes());
    mac.update(b":");
    mac.update(value.as_bytes());
    format!("{kind}:{}", hex::encode(mac.finalize().into_bytes()))
}

/// A 429 in Anthropic's error shape. OpenAI clients read the same
/// `error.message` and `error.type` fields.
fn too_many_requests(message: String, retry_after_seconds: Option<f64>) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": {"type": "rate_limit_error", "message": message},
    });
    let mut builder = Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("content-type", "application/json");
    if let Some(wait) = retry_after_seconds {
        builder = builder.header("retry-after", (wait.ceil() as u64).max(1).to_string());
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("static response")
}

/// `Some(429)` when the spend budget for the current period is used up.
pub(crate) fn budget_refusal(state: &AppState) -> Option<Response> {
    let tracker = &state.cost_tracker;
    let (allowed, _) = tracker.check_budget();
    if allowed {
        return None;
    }
    let period = &state.config.budget_period;
    tracing::warn!(
        event = "budget_exceeded",
        period = %period,
        spent_usd = tracker.get_period_cost(),
        "refusing a generation request: spend budget used up"
    );
    Some(too_many_requests(
        format!("Budget exceeded for {period} period"),
        None,
    ))
}

fn rate_limited(what: &str, wait_seconds: f64) -> Response {
    crate::observability::proxy_counters::record_rate_limited("headroom");
    too_many_requests(
        format!("{what} rate limited. Retry after {wait_seconds:.1}s"),
        Some(wait_seconds),
    )
}

/// Input tokens a request asks for, estimated before anything is sent.
/// Anthropic bodies go through the `count_tokens` estimator, which weighs
/// images by size rather than by their base64 text; other shapes count the
/// whole body with the model's tokenizer.
fn estimate_request_tokens(path: &str, body: &[u8]) -> u64 {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let model = gemini_generate_model(path)
        .or_else(|| parsed.get("model").and_then(Value::as_str))
        .unwrap_or("");
    if path.ends_with("/v1/messages") && parsed.is_object() {
        crate::handlers::count_tokens::estimate_input_tokens(&parsed, model)
    } else {
        headroom_core::tokenizer::get_tokenizer(model).count_text(&String::from_utf8_lossy(body))
            as u64
    }
}

/// Middleware: refuse a generation request over the request rate, the spend
/// budget or the token rate, in that order (upstream's handler order).
pub(crate) async fn admission_gate(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if req.method() != Method::POST || !is_generation_path(req.uri().path()) {
        return next.run(req).await;
    }
    let limiter = state.rate_limiter.clone();
    let key = limiter
        .as_ref()
        .map(|_| rate_limit_key(req.headers()))
        .unwrap_or_default();

    if let Some(limiter) = &limiter {
        let r = limiter.check_request(&key);
        if !r.allowed {
            return rate_limited("Request", r.wait_seconds);
        }
    }
    if let Some(refusal) = budget_refusal(&state) {
        return refusal;
    }
    let Some(limiter) = limiter.filter(|l| l.limits_tokens()) else {
        return next.run(req).await;
    };

    // Only a TPM limit needs the body, so only then is it buffered here.
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, state.config.max_body_bytes as usize).await {
        Ok(b) => b,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::PAYLOAD_TOO_LARGE)
                .body(Body::from(format!("request body: {e}")))
                .expect("static response");
        }
    };
    let tokens = estimate_request_tokens(parts.uri.path(), &bytes);
    let r = limiter.check_tokens(&key, u32::try_from(tokens).unwrap_or(u32::MAX));
    if !r.allowed {
        return rate_limited("Token", r.wait_seconds);
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_paths() {
        for p in [
            "/v1/messages",
            "/anthropic/v1/messages",
            "/v1/chat/completions",
            "/v1/responses",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
        ] {
            assert!(is_generation_path(p), "{p}");
        }
        for p in [
            "/v1/messages/count_tokens",
            "/v1/models",
            "/v1beta/models/gemini-2.5-pro:countTokens",
            "/v1beta/models/gemini-2.5-pro:batchGenerateContent",
        ] {
            assert!(!is_generation_path(p), "{p}");
        }
    }

    #[test]
    fn rate_limit_key_is_opaque_and_per_credential() {
        let mut a = HeaderMap::new();
        a.insert("api-key", "sk-same-prefix-AAAA".parse().unwrap());
        let mut b = HeaderMap::new();
        b.insert("api-key", "sk-same-prefix-BBBB".parse().unwrap());
        let ka = rate_limit_key(&a);
        assert_ne!(ka, rate_limit_key(&b));
        assert_eq!(ka, rate_limit_key(&a));
        assert!(!ka.contains("sk-same"), "{ka}");
        assert!(ka.starts_with("api-key:"));

        // Authorization wins over the other two.
        a.insert("authorization", "Bearer x".parse().unwrap());
        assert!(rate_limit_key(&a).starts_with("authorization:"));
        assert_eq!(rate_limit_key(&HeaderMap::new()), "default");
    }
}
