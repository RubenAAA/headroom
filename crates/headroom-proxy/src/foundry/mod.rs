//! Azure AI Foundry provider routing — port of the Python Foundry support.
//!
//! # What this module owns
//!
//! Azure AI Foundry (AI Services) hosts the Anthropic-format Claude API
//! at:
//!
//! ```text
//! https://{resource}.services.ai.azure.com/anthropic
//! ```
//!
//! Claude Code in Foundry mode (`CLAUDE_CODE_USE_FOUNDRY=1`) points the
//! Anthropic SDK at `ANTHROPIC_FOUNDRY_BASE_URL`, and the SDK appends
//! `/v1/messages` — so the proxy receives:
//!
//! ```text
//! POST /anthropic/v1/messages
//! ```
//!
//! The Python reference (`headroom/providers/proxy_routes.py`,
//! `foundry_anthropic_messages`) normalizes that path to `/v1/messages`
//! and forwards to the resolved Anthropic target. The target resolution
//! (`headroom/providers/registry.py::resolve_api_overrides`) lets
//! `ANTHROPIC_FOUNDRY_BASE_URL` fill the Anthropic upstream when no
//! explicit target was given, and `headroom/cli/wrap.py::_foundry_upstream_url`
//! derives that URL from `ANTHROPIC_FOUNDRY_RESOURCE` when only the
//! resource name is set.
//!
//! # Rust mapping
//!
//! The Rust proxy has a single mandatory `--upstream`, so the Python
//! "env fills the unset Anthropic target" shape maps to a
//! Foundry-route-scoped upstream override:
//!
//! - `POST /anthropic/v1/messages` is rewritten to `/v1/messages`
//!   (query preserved) and flows through the same
//!   [`crate::proxy::forward_http`] pipeline as the plain Anthropic
//!   route — compression gate, SSE telemetry tee, streaming and
//!   non-streaming forwarding are all shared, exactly as Python
//!   reuses `handle_anthropic_messages`.
//! - When `Config::foundry_base_url` is resolved (explicit
//!   `--foundry-base-url` / `ANTHROPIC_FOUNDRY_BASE_URL`, or derived
//!   from `--foundry-resource` / `ANTHROPIC_FOUNDRY_RESOURCE`), the
//!   Foundry route forwards there instead of `--upstream`. Requests on
//!   every other path are unaffected.
//!
//! # Auth
//!
//! Mirrors Python: no Foundry-specific auth handling. The client's own
//! credentials — `api-key` (Foundry key auth) or `Authorization:
//! Bearer <AAD token>` — pass through verbatim via
//! [`crate::headers::build_forward_request_headers`], which strips only
//! hop-by-hop and internal `x-headroom-*` headers.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, Response, Uri};
use axum::response::IntoResponse;

use crate::proxy::{forward_http, AppState, UpstreamOverride};

/// Derive the Azure AI Foundry endpoint URL from a resource name.
///
/// Port of `headroom/cli/wrap.py::_foundry_upstream_url`: Azure AI
/// Foundry (AI Services) hosts the Anthropic-format Claude API at
/// `https://{resource}.services.ai.azure.com/anthropic`, which matches
/// the URL Claude Code constructs internally from
/// `ANTHROPIC_FOUNDRY_RESOURCE`.
pub fn foundry_upstream_url(resource: &str) -> String {
    format!(
        "https://{}.services.ai.azure.com/anthropic",
        resource.trim()
    )
}

/// Resolve the Foundry upstream base URL from config inputs.
///
/// Mirrors the Python precedence (`resolve_api_overrides` +
/// `wrap.py`): an explicit base URL wins; otherwise derive from the
/// resource name; otherwise Foundry routing has no dedicated upstream
/// and the route falls back to `--upstream`.
pub fn resolve_foundry_base_url(
    base_url: Option<url::Url>,
    resource: Option<&str>,
) -> Option<url::Url> {
    if base_url.is_some() {
        return base_url;
    }
    let resource = resource.map(str::trim).filter(|r| !r.is_empty())?;
    // The derived URL is well-formed by construction for any non-empty
    // resource name; a parse failure means the operator passed
    // something that can't be a hostname label, which we surface as
    // "no Foundry upstream" plus a WARN rather than a boot failure.
    match url::Url::parse(&foundry_upstream_url(resource)) {
        Ok(u) => Some(u),
        Err(e) => {
            tracing::warn!(
                event = "foundry_resource_invalid",
                resource = %resource,
                error = %e,
                "ANTHROPIC_FOUNDRY_RESOURCE does not form a valid URL; \
                 Foundry route will use --upstream"
            );
            None
        }
    }
}

/// Rewrite `/anthropic/v1/messages` to `/v1/messages`, preserving the
/// query string. Port of the `normalize_request_path` call in the
/// Python `foundry_anthropic_messages` route.
fn rewrite_to_messages_path(uri: &Uri) -> Uri {
    let path_and_query = match uri.query() {
        Some(q) => format!("/v1/messages?{q}"),
        None => "/v1/messages".to_string(),
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(
        path_and_query
            .parse()
            .expect("static path + previously-valid query parses"),
    );
    Uri::from_parts(parts).expect("rebuilt URI from valid parts")
}

/// POST handler for `/anthropic/v1/messages` (Azure AI Foundry shape).
///
/// Normalizes the path to `/v1/messages` so the shared
/// [`forward_http`] pipeline (compression gate, streaming/SSE tee)
/// treats the request identically to the plain Anthropic route, then
/// forwards to `Config::foundry_base_url` when configured (falling
/// back to `--upstream`). Streaming vs non-streaming needs no
/// dispatch here — `forward_http` streams SSE responses through
/// unchanged, same as `/v1/messages`.
pub async fn handle_foundry_messages(
    State(state): State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    mut req: Request<Body>,
) -> Response<Body> {
    let rewritten = rewrite_to_messages_path(req.uri());
    tracing::debug!(
        event = "foundry_path_normalized",
        original_path = %req.uri().path(),
        normalized_path = %rewritten.path(),
        foundry_upstream = ?state.config.foundry_base_url.as_ref().map(url::Url::as_str),
        "Foundry request path normalized to /v1/messages"
    );
    *req.uri_mut() = rewritten;
    if let Some(base) = state.config.foundry_base_url.clone() {
        req.extensions_mut().insert(UpstreamOverride(base));
    }
    forward_http(state, client_addr, req)
        .await
        .unwrap_or_else(|e| e.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors tests/test_azure_foundry_claude_compression.py
    // (test_foundry_upstream_url_*).
    #[test]
    fn foundry_upstream_url_builds_services_endpoint() {
        assert_eq!(
            foundry_upstream_url("my-org-claude"),
            "https://my-org-claude.services.ai.azure.com/anthropic"
        );
    }

    #[test]
    fn foundry_upstream_url_strips_whitespace() {
        assert_eq!(
            foundry_upstream_url("  my-resource  "),
            "https://my-resource.services.ai.azure.com/anthropic"
        );
    }

    #[test]
    fn foundry_upstream_url_preserves_hyphens_and_digits() {
        assert_eq!(
            foundry_upstream_url("avanade-claude-42"),
            "https://avanade-claude-42.services.ai.azure.com/anthropic"
        );
    }

    // Mirrors resolve_api_overrides precedence: explicit base URL wins
    // over the resource-derived URL.
    #[test]
    fn resolve_prefers_explicit_base_url_over_resource() {
        let explicit: url::Url = "https://gw.example.com/anthropic".parse().unwrap();
        let out = resolve_foundry_base_url(Some(explicit.clone()), Some("my-resource"));
        assert_eq!(out, Some(explicit));
    }

    #[test]
    fn resolve_derives_from_resource_when_no_base_url() {
        let out = resolve_foundry_base_url(None, Some("my-org-claude"));
        assert_eq!(
            out.unwrap().as_str(),
            "https://my-org-claude.services.ai.azure.com/anthropic"
        );
    }

    #[test]
    fn resolve_none_when_unconfigured() {
        assert_eq!(resolve_foundry_base_url(None, None), None);
        // Blank resource behaves like unset (Python strips whitespace).
        assert_eq!(resolve_foundry_base_url(None, Some("   ")), None);
    }

    #[test]
    fn rewrite_strips_anthropic_prefix_and_keeps_query() {
        let uri: Uri = "/anthropic/v1/messages?beta=true".parse().unwrap();
        assert_eq!(
            rewrite_to_messages_path(&uri).to_string(),
            "/v1/messages?beta=true"
        );

        let bare: Uri = "/anthropic/v1/messages".parse().unwrap();
        assert_eq!(rewrite_to_messages_path(&bare).to_string(), "/v1/messages");
    }
}
