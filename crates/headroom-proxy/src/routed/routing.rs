//! Route resolution for the routed paths.
//!
//! The cost-aware router rewrite runs first so a rewritten id still meets the
//! table below; then the legacy local-model alias and the model-routes table
//! pick the upstream. Also owns the fallback re-dispatch when a routed
//! upstream refuses a turn the router moved.

use crate::proxy::{forward_http, AppState};
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::Value;
use std::net::SocketAddr;

/// The request after the cost-aware router ran.
pub(crate) struct ModelRouting {
    pub parsed: Value,
    pub body: Bytes,
    /// The model the client asked for, when the router rewrote it below.
    /// Everything keyed on conversation identity (session key, prefix replay,
    /// roster pin, prefix fingerprint) must derive from this, so a rerouted
    /// turn stays on the key its conversation already has. `None` when the
    /// client routed itself.
    pub identity_model: Option<String>,
}

/// Cost-aware model routing (#1706): the same helper the passthrough path
/// uses, applied here so a rewritten id still meets the route table below.
/// A rule sending small tool-less turns at a `claude-*` alias routes them
/// to that alias's upstream. Disabled by default; when no rule matches the
/// bytes come back untouched.
///
/// Runs after the sidecar block on purpose: a sidecar is answered and gone
/// before this line, so routing can never claim one. On a rewrite both
/// `parsed` and `body` move together, so the passthrough and no-match
/// paths below see the model actually being sent — and a second
/// application downstream is a no-op (the id already equals the target).
/// Any serialization failure skips routing: it is an optimization, never
/// a breakage.
pub(crate) fn apply_model_routing(
    state: &AppState,
    mut parsed: Value,
    mut body: Bytes,
    request_id: &str,
) -> ModelRouting {
    let mut identity_model: Option<String> = None;
    {
        let router = crate::model_router::ModelRouter::new(Some(state.config.model_router.clone()));
        if router.enabled() {
            if let Ok(buf) = serde_json::to_vec(&parsed) {
                // Cooldowns are consulted here as well as in `forward_http`:
                // a target parked by an earlier fallback must be passed over
                // on this path too, or the routed upstream that just failed
                // would be probed again on the very next turn.
                let routed = crate::model_router::apply_to_anthropic_body_with_cooldowns(
                    Bytes::from(buf),
                    &router,
                    request_id,
                    Some(&state.model_route_cooldowns),
                );
                if let Ok(next) = serde_json::from_slice::<Value>(&routed) {
                    let before = parsed
                        .get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let after = next
                        .get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if !after.is_empty() && after != before {
                        identity_model = Some(before.to_string());
                        parsed = next;
                        body = routed;
                    }
                }
            }
        }
    }
    ModelRouting {
        parsed,
        body,
        identity_model,
    }
}

/// Where a routed model id goes: its upstream and how to speak to it.
pub(crate) struct RouteTarget {
    pub upstream: url::Url,
    pub translate: bool,
    pub target_model: Option<String>,
    pub auth_env: Option<String>,
}

/// Find a matching route: first the legacy `local_model` alias (backward
/// compat), then the model-routes table. `None` means no route claims this
/// model and the turn falls through to the standard forwarder.
pub(crate) fn find_route_target(
    config: &crate::config::Config,
    body_model: &str,
) -> Option<RouteTarget> {
    let matched =
        if let (Some(model), Some(upstream)) = (&config.local_model, &config.local_upstream) {
            if body_model == model.as_str() {
                Some((upstream.clone(), true, None, None))
            } else {
                None
            }
        } else {
            None
        };

    let matched = matched.or_else(|| {
        config
            .model_routes
            .iter()
            .find(|r| r.matches(body_model))
            .and_then(|r| {
                Some((
                    r.upstream.clone()?,
                    r.translate,
                    r.target_model.clone(),
                    r.auth_env.clone(),
                ))
            })
    });

    matched.map(
        |(upstream, translate, target_model, auth_env)| RouteTarget {
            upstream,
            translate,
            target_model,
            auth_env,
        },
    )
}

/// Re-dispatch a turn whose routed upstream failed to the client's own model.
///
/// The body handed on is `parsed` — the request as it stood after the CTX
/// stages ran, with only the model put back. Re-serialising the client's
/// original would throw away the compression and the parked replay prefix that
/// the first attempt already committed to, and the second attempt has to close
/// those out under the same request id. [`SkipModelRouting`] carries that id
/// and keeps `forward_http` from applying the rules that sent this turn to the
/// upstream that just refused it.
///
/// [`SkipModelRouting`]: crate::proxy::SkipModelRouting
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_route_fallback(
    state: AppState,
    client_addr: SocketAddr,
    method: Method,
    uri: Uri,
    mut headers: HeaderMap,
    mut parsed: Value,
    client_model: &str,
    request_id: &str,
) -> Response {
    if let Some(obj) = parsed.as_object_mut() {
        obj.insert("model".to_string(), Value::String(client_model.to_string()));
    }
    let body = match serde_json::to_vec(&parsed) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                event = "handler_error",
                handler = "messages_local_model",
                error = %e,
                "failed to serialise the fallback body"
            );
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("internal handler error"))
                .expect("static response");
        }
    };
    // The routed attempt rewrote the body; the client's length no longer
    // describes it, and a stale one would truncate the fallback request.
    headers.remove(http::header::CONTENT_LENGTH);
    // No gate here on purpose: this body was deliberately unredacted above so
    // the fallback forwards what the client sent rather than placeholders.
    // Re-redacting would send placeholders to the fallback upstream under a
    // fresh session — exactly what unredact_body exists to prevent.
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(hs) = builder.headers_mut() {
        *hs = headers;
    }
    let mut req = match builder.body(Body::from(body)) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                event = "handler_error",
                handler = "messages_local_model",
                error = %e,
                "failed to build the fallback request"
            );
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("internal handler error"))
                .expect("static response");
        }
    };
    req.extensions_mut()
        .insert(crate::proxy::SkipModelRouting(request_id.to_string()));
    forward_http(state, client_addr, req)
        .await
        .unwrap_or_else(|e| e.into_response())
}
