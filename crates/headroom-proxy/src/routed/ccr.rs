//! Proxy-tool resolution for the routed paths.
//!
//! This path injects `headroom_retrieve` (and memory tools), so it owns
//! running the calls the model makes, in the upstream's own shape.

use axum::http::HeaderMap;
use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;

/// Resolve `headroom_retrieve` on a buffered routed reply, in the upstream's
/// own shape. Returns the resolved response and the usage of the rounds the
/// client never saw.
pub(crate) async fn resolve_routed_ccr(
    response: &Value,
    ccr: &RoutedCcr,
) -> (Value, crate::proxy::CcrRoundUsage) {
    let provider = if ccr.responses_shape {
        "openai_responses"
    } else {
        "openai"
    };
    let Ok(url) = url::Url::parse(&ccr.upstream_url) else {
        tracing::warn!(
            event = "routed_ccr_bad_upstream_url",
            url = %ccr.upstream_url,
            "cannot resolve headroom_retrieve on this turn"
        );
        return (response.clone(), crate::proxy::CcrRoundUsage::default());
    };
    let body = match serde_json::to_vec(response) {
        Ok(b) => Bytes::from(b),
        Err(_) => return (response.clone(), crate::proxy::CcrRoundUsage::default()),
    };
    let (resolved, usage) = crate::proxy::handle_ccr_response(
        &body,
        &ccr.request_body,
        &url,
        &ccr.client,
        ccr.store.as_ref(),
        ccr.stores.as_ref(),
        &ccr.config,
        &ccr.request_id,
        &ccr.headers,
        provider,
        ccr.redact.clone(),
    )
    .await;
    match serde_json::from_slice(&resolved) {
        Ok(v) => (v, usage),
        Err(_) => (response.clone(), usage),
    }
}

/// Run both resolvers over a routed turn until a pass changes nothing.
///
/// Each of them only runs the calls standing when it starts, and either one's
/// continuation can come back asking for the other — a `memory_search` the
/// proxy answered can leave the model asking for a `headroom_retrieve` that
/// retrieval has already passed by. Running them once each, in a fixed order,
/// left that call with nobody to run it. See `MAX_RESOLVER_ALTERNATIONS`.
pub(crate) async fn resolve_routed_proxy_tools(
    response: &Value,
    ccr: &RoutedCcr,
) -> (Value, crate::proxy::CcrRoundUsage) {
    let mut body = response.clone();
    let mut rounds = crate::proxy::CcrRoundUsage::default();
    for _ in 0..crate::proxy::MAX_RESOLVER_ALTERNATIONS {
        let before = body.clone();

        let (next, ccr_rounds) = resolve_routed_ccr(&body, ccr).await;
        rounds.absorb(ccr_rounds);
        let (next, mem_rounds) = resolve_routed_memory(&next, ccr).await;
        rounds.absorb(mem_rounds);
        body = next;

        if body == before {
            break;
        }
    }
    (body, rounds)
}

/// Run any `memory_*` call the model made, in the upstream's own shape.
///
/// The twin of [`resolve_routed_ccr`]. `handle_memory_response` was already
/// provider-aware — it knows the Responses API keeps its items under `input`
/// rather than `messages` — but only the two Anthropic seams ever called it,
/// so on this path the injected tools had no one to run them.
pub(crate) async fn resolve_routed_memory(
    response: &Value,
    ccr: &RoutedCcr,
) -> (Value, crate::proxy::CcrRoundUsage) {
    let Some(memory) = ccr.memory.as_ref() else {
        return (response.clone(), crate::proxy::CcrRoundUsage::default());
    };
    let provider = if ccr.responses_shape {
        "openai_responses"
    } else {
        "openai"
    };
    let Ok(url) = url::Url::parse(&ccr.upstream_url) else {
        tracing::warn!(
            event = "routed_memory_bad_upstream_url",
            url = %ccr.upstream_url,
            "cannot resolve a memory tool call on this turn"
        );
        return (response.clone(), crate::proxy::CcrRoundUsage::default());
    };
    let body = match serde_json::to_vec(response) {
        Ok(b) => Bytes::from(b),
        Err(_) => return (response.clone(), crate::proxy::CcrRoundUsage::default()),
    };
    let (resolved, usage) = crate::proxy::handle_memory_response(
        &body,
        &ccr.request_body,
        &url,
        &ccr.client,
        memory,
        &ccr.config,
        &ccr.request_id,
        &ccr.headers,
        provider,
        ccr.redact.clone(),
    )
    .await;
    match serde_json::from_slice(&resolved) {
        Ok(v) => (v, usage),
        Err(_) => (response.clone(), usage),
    }
}

/// What the routed response arms need to resolve a `headroom_retrieve` call.
///
/// Assembled once at the dispatch point because the request shape, URL and
/// headers are only in scope there.
pub(crate) struct RoutedCcr {
    pub store: Arc<dyn headroom_core::ccr::CcrStore>,
    /// Per-project content stores, for the cold-tier lookup when `store`
    /// has expired a block. `None` disables the fallback.
    pub stores: Option<Arc<crate::ctx::projects::ProjectStores>>,
    /// Memory tools are injected on this path too (see the injection site in
    /// `apply_ctx_request_transforms`), so this path has to run them. Without
    /// it the call streamed on to the client, which has never heard of
    /// `memory_search` and answered `No such tool available: memory_search`.
    pub memory: Option<crate::proxy::MemoryToolContext>,
    pub client: reqwest::Client,
    pub upstream_url: String,
    pub headers: HeaderMap,
    /// The request as translated and sent upstream, in the upstream's shape.
    pub request_body: Bytes,
    pub config: Arc<crate::config::Config>,
    pub request_id: String,
    /// True when the upstream is the Responses API rather than
    /// chat-completions. The two disagree about where tool calls live.
    pub responses_shape: bool,
    /// Redaction memory for continuations this turn: content fetched mid-turn
    /// (memory answers, cold-tier blocks) is redacted before it joins an
    /// upstream-bound continuation. `None` when the outbound body needed no
    /// redaction — or the flag is off — and continuations pass through.
    pub redact: Option<crate::redact::RedactRef>,
}

impl RoutedCcr {
    /// Assemble what the routed response arms need to resolve the model's
    /// tool calls. Built at the dispatch point because the request shape, URL
    /// and headers are only in scope there.
    ///
    /// This path injects `headroom_retrieve` (above) and hands compression a
    /// CCR store, so it owns resolving the calls the model makes. Present only
    /// when the store exists; without it there is nothing to look a hash up in.
    /// Scoped off the client's own request: that is where the system prompt
    /// with the working directory lives, and the memory partition is keyed on
    /// it. The translated body upstream no longer carries it in that shape.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn assemble(
        state: &crate::proxy::AppState,
        headers: &HeaderMap,
        parsed: &Value,
        upstream_url: String,
        upstream_headers: HeaderMap,
        request_body: Bytes,
        request_id: &str,
        responses_shape: bool,
        redact_session_key: &str,
    ) -> Option<RoutedCcr> {
        let routed_memory = crate::proxy::memory_tool_context(
            state,
            &Some(headers.clone()),
            Some("openai"),
            &serde_json::to_vec(parsed)
                .map(Bytes::from)
                .unwrap_or_default(),
        )
        .await;
        let ccr_stores = state.ctx_offload.as_ref().map(|r| r.store.stores());
        state.ccr_store().map(|store| RoutedCcr {
            store,
            stores: ccr_stores,
            memory: routed_memory,
            client: state.client.clone(),
            upstream_url,
            headers: upstream_headers,
            request_body,
            config: state.config.clone(),
            request_id: request_id.to_string(),
            responses_shape,
            // Continuations inherit the turn's redaction: memory and cold-tier
            // content fetched mid-turn is redacted before it goes back upstream.
            // Gated on the flag, not on outbound spans — a clean prompt can
            // still retrieve secrets mid-turn.
            redact: state
                .config
                .redact_sensitive
                .then(|| crate::redact::RedactRef {
                    store: state.redact_store.clone(),
                    session_key: redact_session_key.to_string(),
                }),
        })
    }
}

/// The handoff between the two resolvers, on the shape that broke in the field.
///
/// A turn asks for `memory_search`; the proxy answers it and continues; the
/// model comes back asking for `headroom_retrieve`. Retrieval has already had
/// its turn by then, so before the alternation loop that second call stood
/// unresolved — the splice dropped it and downgraded the turn to `end_turn`,
/// and the user got an apology instead of the content.
#[cfg(test)]
mod resolver_alternation_tests {
    use super::*;
    use headroom_core::ccr::backends::InMemoryCcrStore;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HASH: &str = "abc123def456abc123def456";
    const CONTENT: &str = "the original large content";

    fn memory_ctx() -> crate::proxy::MemoryToolContext {
        let mut handler = crate::memory::handler::MemoryHandler::new(
            crate::memory::handler::MemoryConfig {
                enabled: true,
                ..Default::default()
            },
            "test",
        );
        // An empty in-process store: what a search returns does not matter
        // here, only that the call resolves and the turn continues.
        handler.set_backend(Arc::new(
            crate::memory::local_backend::LocalMemoryBackend::new(),
        ));
        crate::proxy::MemoryToolContext {
            handler: Arc::new(handler),
            provider: crate::memory::tool_adapter::Provider::Openai,
            user_id: "u1".to_string(),
        }
    }

    /// One chat-completions turn carrying a single tool call by name.
    fn turn_calling(name: &str, arguments: &str) -> Value {
        serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5},
        })
    }

    #[tokio::test]
    async fn a_retrieval_asked_for_by_a_memory_continuation_still_runs() {
        let store = InMemoryCcrStore::new();
        headroom_core::ccr::CcrStore::put(&store, HASH, CONTENT);

        let server = MockServer::start().await;
        // First continuation — the answer to `memory_search`. The model uses it
        // to decide it now wants the offloaded content.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(turn_calling(
                "headroom_retrieve",
                &format!("{{\"hash\":\"{HASH}\"}}"),
            )))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Second continuation — the answer to `headroom_retrieve`. Reaching
        // this at all is the thing under test.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "done"},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 11, "completion_tokens": 6},
            })))
            .mount(&server)
            .await;

        let ccr = RoutedCcr {
            stores: None,
            store: Arc::new(store),
            memory: Some(memory_ctx()),
            client: reqwest::Client::new(),
            upstream_url: format!("{}/v1/chat/completions", server.uri()),
            headers: HeaderMap::new(),
            request_body: Bytes::from(
                serde_json::to_vec(&serde_json::json!({
                    "model": "gpt-x",
                    "messages": [{"role": "user", "content": "hi"}],
                }))
                .unwrap(),
            ),
            config: Arc::new(crate::config::Config::for_test(
                server.uri().parse().unwrap(),
            )),
            request_id: "req-alt".to_string(),
            responses_shape: false,
            redact: None,
        };

        let opening = turn_calling("memory_search", "{\"query\":\"anything\"}");
        let (resolved, rounds) = resolve_routed_proxy_tools(&opening, &ccr).await;

        let message = &resolved["choices"][0]["message"];
        assert_eq!(
            message["content"], "done",
            "the second continuation never ran: {resolved}"
        );
        assert!(
            message
                .get("tool_calls")
                .is_none_or(|c| c.as_array().is_none_or(std::vec::Vec::is_empty)),
            "a proxy tool call survived to the client: {resolved}"
        );
        assert_eq!(
            rounds.rounds, 2,
            "both continuations are billed and neither reaches the client"
        );
    }

    /// A turn with nothing for either resolver must not cost an upstream call,
    /// or the loop would tax every ordinary turn for a case that is rare.
    #[tokio::test]
    async fn a_turn_needing_neither_resolver_makes_no_upstream_call() {
        let server = MockServer::start().await;
        let ccr = RoutedCcr {
            stores: None,
            store: Arc::new(InMemoryCcrStore::new()),
            memory: Some(memory_ctx()),
            client: reqwest::Client::new(),
            upstream_url: format!("{}/v1/chat/completions", server.uri()),
            headers: HeaderMap::new(),
            request_body: Bytes::from(b"{}".to_vec()),
            config: Arc::new(crate::config::Config::for_test(
                server.uri().parse().unwrap(),
            )),
            request_id: "req-none".to_string(),
            responses_shape: false,
            redact: None,
        };

        let plain = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop",
            }]
        });
        let (resolved, rounds) = resolve_routed_proxy_tools(&plain, &ccr).await;

        assert_eq!(resolved, plain);
        assert_eq!(rounds.rounds, 0);
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "an idle pass must not call upstream"
        );
    }

}
