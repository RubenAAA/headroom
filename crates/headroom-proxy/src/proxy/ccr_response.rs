//! CCR continuation task: retrieval fetch, continuation send/read, and
//! residual resolution.
//!
//! Pure module move of the round-loop body of
//! [`super::handle_ccr_response`]: per-call fetch (query path, store
//! hit, cold-tier recovery), round fate (mixed/all-failed splices),
//! continuation build/send/read, and the unresolved-residual fallback.
//! No logic changes — every block is verbatim motion.
//!
//! `use super::*` keeps the parent's private items
//! (`CcrRoundUsage`, `check_continuation_prefix`,
//! `continuation_cut_retryable`, …) reachable with zero visibility
//! churn elsewhere in the crate.

use super::*;
use headroom_core::ccr::response_handler::{CcrToolCall, CcrToolResult};
use std::collections::HashMap;
use std::sync::Arc;

/// Stamp retrieved content with its marker and build the success
/// result. The stamp is proxy prose and must survive redaction
/// verbatim, so redaction always runs first.
/// Extracted from `handle_ccr_response` without behavior change.
fn retrieved_result(
    call: &CcrToolCall,
    content: String,
    redact: &Option<crate::redact::RedactRef>,
) -> CcrToolResult {
    let content = match redact.as_ref() {
        Some(r) => crate::redact::redact_string(r, &content),
        None => content,
    };
    // Stamp after redact: proxy prose, must survive verbatim.
    let content = format!(
        "{}\n\n{content}",
        headroom_core::ccr::response_handler::retrieved_content_stamp(&call.hash_key)
    );
    CcrToolResult {
        tool_call_id: call.tool_call_id.clone(),
        content,
        success: true,
        items_retrieved: 1,
    }
}

/// Keyword-search path: the model called without a marker hash. None
/// of the hash machinery applies (no plausibility gate, no cold tier
/// — the index IS the store here). Searches the current project's
/// content index only: the sweep-everything fallback is a
/// miss-recovery tool, not a search scope.
#[allow(clippy::too_many_arguments)]
pub(super) async fn answer_query_call(
    query: &str,
    call: &CcrToolCall,
    stores: Option<&Arc<crate::ctx::projects::ProjectStores>>,
    outgoing_headers: &http::HeaderMap,
    current_request: &serde_json::Value,
    config: &Config,
    redact: &Option<crate::redact::RedactRef>,
    request_id: &str,
    round: usize,
) -> CcrToolResult {
    let project = resolve_ctx_project(
        Some(outgoing_headers),
        current_request,
        config.memory_project_root.as_deref(),
    );
    tracing::info!(
        request_id = %request_id,
        round = round + 1,
        query = %query.chars().take(80).collect::<String>(),
        event = "ccr_retrieval_call",
        "ccr: model asked by query"
    );
    // sqlite reads block: same blocking pool as the cold
    // tier, so the tokio worker driving this turn never
    // stalls on a search.
    let hits = match stores {
        Some(stores) => {
            let stores = Arc::clone(stores);
            let query_for_search = query.to_string();
            tokio::task::spawn_blocking(move || {
                let Some(store) = stores.content(&project) else {
                    return Vec::new();
                };
                store
                    .search(
                        std::slice::from_ref(&query_for_search),
                        &headroom_core::ctx::SearchOpts {
                            limit: 3,
                            source: None,
                            content_type: None,
                            sort: headroom_core::ctx::SortMode::Relevance,
                        },
                    )
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default()
        }
        None => Vec::new(),
    };
    crate::observability::ctx_metrics::observe_retrieval(!hits.is_empty());
    if hits.is_empty() {
        return CcrToolResult {
            tool_call_id: call.tool_call_id.clone(),
            content: format!(
                "Error: no indexed content matched query '{query}'. \
                 Try different keywords, or a hash from a compression marker."
            ),
            success: false,
            items_retrieved: 0,
        };
    }
    let mut content = format!("Top {} indexed match(es) for query '{query}':", hits.len());
    for hit in &hits {
        content.push_str(&format!("\n\n### {}\n{}", hit.title, hit.content));
    }
    let content = match redact.as_ref() {
        Some(r) => crate::redact::redact_string(r, &content),
        None => content,
    };
    // Link the answer to the query that asked for it, so a
    // later turn re-reading history references this message
    // instead of paying another continuation round.
    let content = format!(
        "{}\n\n{content}",
        headroom_core::ccr::response_handler::retrieved_query_stamp(query)
    );
    CcrToolResult {
        tool_call_id: call.tool_call_id.clone(),
        content,
        success: true,
        items_retrieved: hits.len(),
    }
}

/// Store-hit path: warn when the file went stale since the read, then
/// redact and stamp. What comes back is the file as it was when it was
/// read. If it has been edited since, say so — otherwise the model
/// takes pre-edit content for the current file and acts on it.
///
/// Continuation bodies go back upstream: redact what the store
/// returned through this turn's map first. Store hits are usually
/// already redacted (offload ran post-redact) and re-redacting is
/// idempotent; cold-tier recoveries may not be.
pub(super) fn finish_store_hit(
    call: &CcrToolCall,
    content: String,
    stale_reads: &HashMap<String, String>,
    redact: &Option<crate::redact::RedactRef>,
    request_id: &str,
) -> CcrToolResult {
    // What comes back is the file as it was when it was read. If
    // it has been edited since, say so — otherwise the model
    // takes pre-edit content for the current file and acts on it.
    let content = match stale_reads.get(&call.hash_key) {
        Some(path) => {
            tracing::info!(
                request_id = %request_id,
                hash = %call.hash_key,
                "ccr: retrieved a Read that has since gone stale"
            );
            format!(
                "{}\n\n{content}",
                crate::compression::ctx_offload::stale_read_warning(path)
            )
        }
        None => content,
    };
    // Continuation bodies go back upstream: redact what the
    // store returned through this turn's map first. Store hits
    // are usually already redacted (offload ran post-redact)
    // and re-redacting is idempotent; cold-tier recoveries may
    // not be.
    let result = retrieved_result(call, content, redact);
    tracing::debug!(
        request_id = %request_id,
        hash = %call.hash_key,
        "ccr: retrieved original content"
    );
    result
}

/// Same-project fast path for a store miss: the cross-project sweep
/// deliberately skips the requesting project's own store, but an
/// expired block indexed under the current project is still on disk
/// here. One indexed lookup before opening every project file on
/// disk. A hash the model invented cannot be on disk.
pub(super) async fn recover_local_tier(
    call: &CcrToolCall,
    stores: Option<&Arc<crate::ctx::projects::ProjectStores>>,
    project_from: &str,
    request_id: &str,
) -> Option<(String, String)> {
    // Same-project fast path first: the cross-project sweep
    // below deliberately skips the requesting project's own
    // store, but an expired block indexed under the current
    // project is still on disk here. One indexed lookup
    // before opening every project file on disk.
    let found: Option<(String, String)> = match stores {
        Some(stores)
            if headroom_core::ccr::response_handler::is_plausible_ccr_hash(&call.hash_key) =>
        {
            let stores = Arc::clone(stores);
            let hash = call.hash_key.clone();
            let project = project_from.to_string();
            tokio::task::spawn_blocking(move || {
                stores.content_local(&project, &hash).map(|content| {
                    (
                        headroom_core::ctx::hash_project_dir_canonical(&project),
                        content,
                    )
                })
            })
            .await
            .unwrap_or(None)
        }
        _ => None,
    };
    if let Some((project_to, content)) = found {
        tracing::info!(
            event = "ccr_local_tier_hit",
            request_id = %request_id,
            hash = %call.hash_key,
            project_from = %project_from,
            project_to = %project_to,
            "ccr: missing from the CCR store, recovered from the requesting project's own content index"
        );
        crate::observability::ccr_retrieval::observe_local_tier_hit();
        let content = format!("{CCR_INDEX_RECOVERY_NOTE}\n\n{content}");
        return Some((project_to, content));
    }
    None
}

/// Cross-project cold-tier sweep for a store miss: `ccr.db` keeps a
/// block for an idle week and then drops it, while the per-project
/// content index keeps the same block with no expiry — so a miss here
/// is usually an eviction, and the cold copy is still on disk. The
/// sweep is cross-project because the project that offloaded the block
/// is often not the one asking for it back. Returns the recovered
/// content plus the triage numbers for the miss log.
pub(super) async fn sweep_cross_project(
    call: &CcrToolCall,
    stores: Option<&Arc<crate::ctx::projects::ProjectStores>>,
    project_from: &str,
    request_id: &str,
) -> (Option<(String, String)>, u64, usize, bool) {
    // `ccr.db` keeps a block for an idle week and then drops
    // it. The per-project content index keeps the same block,
    // under the same blake3 key, with no expiry — so a miss
    // here is usually an eviction rather than a block that was
    // never stored, and the cold copy is still on disk. Joining
    // every indexed `content_hash` against the live CCR rows
    // measured this: of blocks indexed inside the TTL window,
    // none were missing from `ccr.db`; of the older ones,
    // 12,807 of 12,843 were. Every miss is an expiry.
    //
    // The sweep is cross-project because the project that
    // offloaded the block is often not the one asking for it
    // back: subagents, teammates and held working directories
    // all move the resolved project between turns. The CCR
    // store itself is one global file and never was sharded by
    // project, so this recovers reach, not isolation.
    let recovered = match stores {
        Some(stores)
            if headroom_core::ccr::response_handler::is_plausible_ccr_hash(&call.hash_key) =>
        {
            // Opening dozens of sqlite files is blocking work.
            // Left inline it stalls the tokio worker driving
            // this turn and every other request on that thread,
            // so it goes to the blocking pool the way the
            // savings ledger already does. A panicked or
            // cancelled join degrades to a plain miss.
            let stores = Arc::clone(stores);
            let hash = call.hash_key.clone();
            let project = project_from.to_string();
            tokio::task::spawn_blocking(move || stores.find_content_any_project(&hash, &project))
                .await
                .map_err(|e| {
                    tracing::warn!(
                        event = "ctx_cold_tier_join_failed",
                        hash = %call.hash_key,
                        error = %e,
                    );
                })
                .ok()
        }
        // A hash the model invented cannot be on disk, and
        // scanning every project for it costs 85 file opens.
        // Both misses seen in production logs were of this
        // shape: one was five characters against the 24-hex
        // format, the other an English sentence.
        Some(_) => {
            tracing::warn!(
                event = "ccr_malformed_hash",
                request_id = %request_id,
                hash = %call.hash_key,
                project_from = %project_from,
                "ccr: retrieval asked for a malformed hash; not a store miss"
            );
            None
        }
        None => None,
    };
    match recovered {
        Some(l) => (l.found, l.elapsed.as_millis() as u64, l.scanned, l.gave_up),
        None => (None, 0, 0, false),
    }
}

/// Full miss path: local-tier recovery, then the cross-project sweep,
/// then a continue-friendly miss note (the old `Error: ... may have
/// been evicted` wording stalled agentic sessions — the model treated
/// it as terminal and retried the hash instead of re-reading the
/// source or using a keyword query).
pub(super) async fn recover_ccr_miss(
    call: &CcrToolCall,
    stores: Option<&Arc<crate::ctx::projects::ProjectStores>>,
    outgoing_headers: &http::HeaderMap,
    current_request: &serde_json::Value,
    config: &Config,
    redact: &Option<crate::redact::RedactRef>,
    request_id: &str,
) -> CcrToolResult {
    let project_from = resolve_ctx_project(
        Some(outgoing_headers),
        current_request,
        config.memory_project_root.as_deref(),
    );
    if let Some((_, content)) = recover_local_tier(call, stores, &project_from, request_id).await {
        return retrieved_result(call, content, redact);
    }
    let (recovered, cold_ms, cold_scanned, cold_gave_up) =
        sweep_cross_project(call, stores, &project_from, request_id).await;
    match recovered {
        Some((project_to, content)) => {
            tracing::info!(
                event = "ccr_cold_tier_hit",
                request_id = %request_id,
                hash = %call.hash_key,
                project_from = %project_from,
                project_to = %project_to,
                cold_tier_ms = cold_ms,
                projects_scanned = cold_scanned,
                "ccr: missing from the CCR store, recovered from the content index"
            );
            crate::observability::ccr_retrieval::observe_cross_project_hit();
            let content = format!("{CCR_INDEX_RECOVERY_NOTE}\n\n{content}");
            retrieved_result(call, content, redact)
        }
        None => {
            // Continue-friendly miss notes (same as the batch
            // path): the old `Error: ... may have been evicted`
            // wording stalled agentic sessions — the model
            // treated it as terminal and retried the hash
            // instead of re-reading the source or using a
            // keyword query. See `missing_ccr_content_note` /
            // `malformed_ccr_hash_note` regression test
            // `miss_notes_stay_continue_friendly`.
            use headroom_core::ccr::response_handler as ccr_rh;
            let content = if ccr_rh::is_plausible_ccr_hash(&call.hash_key) {
                ccr_rh::missing_ccr_content_note(&call.hash_key)
            } else {
                ccr_rh::malformed_ccr_hash_note(&call.hash_key)
            };
            tracing::warn!(
                event = "ccr_content_not_found",
                request_id = %request_id,
                hash = %call.hash_key,
                hash_plausible = ccr_rh::is_plausible_ccr_hash(&call.hash_key),
                project_from = %project_from,
                cross_project_checked = stores.is_some(),
                cold_tier_ms = cold_ms,
                projects_scanned = cold_scanned,
                cold_tier_gave_up = cold_gave_up,
                "ccr: content not found in store"
            );
            CcrToolResult {
                tool_call_id: call.tool_call_id.clone(),
                content,
                success: false,
                items_retrieved: 0,
            }
        }
    }
}

/// Parse the upstream response body. A parse failure skips CCR handling
/// with the pre-replay body.
/// Extracted from `handle_ccr_response` without behavior change.
pub(super) fn parse_ccr_response(
    body_bytes: &bytes::Bytes,
    request_id: &str,
) -> Option<serde_json::Value> {
    match serde_json::from_slice(body_bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::debug!(
                request_id = %request_id,
                error = %e,
                "ccr: failed to parse upstream response as JSON; skipping CCR handling"
            );
            None
        }
    }
}

/// Parse the forwarded request body. A parse failure skips CCR handling
/// with the pre-replay body.
///
/// Called only after the tool-calls gate: a turn with no CCR tool call must
/// not pay a full parse of the forwarded request, which carries the whole
/// conversation and runs on every buffered response.
/// Extracted from `handle_ccr_response` without behavior change.
pub(super) fn parse_ccr_request(
    forwarded_request: &bytes::Bytes,
    request_id: &str,
) -> Option<serde_json::Value> {
    match serde_json::from_slice(forwarded_request) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(
                request_id = %request_id,
                error = %e,
                "ccr: failed to parse original request; skipping CCR handling"
            );
            None
        }
    }
}

/// Round-budget gate: warn and stop when the retrieval rounds are spent.
/// Returns false when the loop ends here.
/// Extracted from `handle_ccr_response` without behavior change.
pub(super) fn check_ccr_round_budget(rounds: usize, max_rounds: usize, request_id: &str) -> bool {
    if rounds >= max_rounds {
        tracing::warn!(
            request_id = %request_id,
            rounds = rounds,
            "ccr: max retrieval rounds reached; returning partial response"
        );
        return false;
    }
    true
}

/// Fetch one CCR call: the keyword-search path when the model called
/// without a marker hash, otherwise the hash path (store hit or cold
/// recovery). An empty hash with no query falls through to the hash
/// path, which reports it as malformed (existing behavior).
#[allow(clippy::too_many_arguments)]
pub(super) async fn fetch_one_ccr_call(
    call: &CcrToolCall,
    ccr_store: &dyn headroom_core::ccr::CcrStore,
    stores: Option<&Arc<crate::ctx::projects::ProjectStores>>,
    outgoing_headers: &http::HeaderMap,
    current_request: &serde_json::Value,
    config: &Config,
    stale_reads: &HashMap<String, String>,
    redact: &Option<crate::redact::RedactRef>,
    request_id: &str,
    round: usize,
) -> CcrToolResult {
    // Keyword-search path: the model called without a marker hash.
    if call.hash_key.is_empty() {
        if let Some(query) = call.query.clone() {
            return answer_query_call(
                &query,
                call,
                stores,
                outgoing_headers,
                current_request,
                config,
                redact,
                request_id,
                round,
            )
            .await;
        }
        // Empty hash and no query: fall through to the hash path,
        // which reports it as malformed (existing behavior).
    }
    // One line per asked hash: sizes repeat-hash waste and names the
    // misses a store-side fix would have to cover.
    tracing::info!(
        request_id = %request_id,
        round = round + 1,
        hash = %call.hash_key,
        event = "ccr_retrieval_call",
        "ccr: model asked for hash"
    );
    let fetched = ccr_store.get(&call.hash_key);
    // Count the tool-driven retrieval here, at the only place both
    // outcomes are known. `/ctx/get` counts the HTTP surface, which
    // nothing on the model path uses, so counting only there left
    // `retrieval_hits` at zero however much the model retrieved.
    crate::observability::ctx_metrics::observe_retrieval(fetched.is_some());
    match fetched {
        Some(content) => finish_store_hit(call, content, stale_reads, redact, request_id),
        None => {
            recover_ccr_miss(
                call,
                stores,
                outgoing_headers,
                current_request,
                config,
                redact,
                request_id,
            )
            .await
        }
    }
}

/// Check the round fate after fetching: mixed CCR + real tool calls get
/// answered in place (the continuation cannot run — appending the
/// assistant message would leave the client's tool_use unanswered
/// upstream); an all-failed round splices in place when every call was
/// replaced, so the client never sees a tool it didn't declare.
/// Returns true when the loop ends here.
/// Extracted from `handle_ccr_response` without behavior change.
#[allow(clippy::too_many_arguments)]
pub(super) fn check_ccr_round_fate(
    handler: &headroom_core::ccr::response_handler::CCRResponseHandler,
    current_response: &mut serde_json::Value,
    results: &[CcrToolResult],
    ccr_count: usize,
    other_count: usize,
    provider: &str,
    request_id: &str,
) -> bool {
    // Mixed CCR + real tool calls. The continuation cannot run: appending
    // the assistant message would leave the client's tool_use unanswered
    // upstream. Skipping used to drop the retrieval with it, so the model
    // asked for content and got nothing — silently on the streamed path,
    // and as a tool_use for a tool it never declared on the buffered one.
    // Answer it in place and let the real tool call go back to the client.
    if other_count > 0 {
        let spliced = handler.splice_ccr_results_as_text(current_response, results, provider);
        tracing::info!(
            request_id = %request_id,
            ccr_count = ccr_count,
            other_count = other_count,
            spliced = spliced,
            "ccr: mixed CCR and real tool calls; answered the retrieval in place"
        );
        crate::observability::ccr_retrieval::observe_outcome(
            crate::observability::ccr_retrieval::OUTCOME_SPLICED_MIXED,
            spliced as u64,
        );
        return true;
    }

    // Every retrieval failed: a continuation would pay a full-prefix
    // round to deliver errors the model can read in place for free.
    // Only break when the splice replaced every call — otherwise the
    // client would get a tool call for a tool it never declared.
    if !results.is_empty() && results.iter().all(|r| !r.success) {
        let spliced = handler.splice_ccr_results_as_text(current_response, results, provider);
        if spliced == ccr_count {
            tracing::info!(
                request_id = %request_id,
                ccr_count = ccr_count,
                "ccr: all retrievals failed; answered in place, skipping continuation"
            );
            crate::observability::ccr_retrieval::observe_outcome(
                crate::observability::ccr_retrieval::OUTCOME_SPLICED_FAILED,
                spliced as u64,
            );
            return true;
        }
    }
    false
}

/// Build the continuation request: append assistant message + tool
/// results (extending sentinel-keyed wrappers instead of pushing them
/// whole), retail the tail breakpoint, and serialize. Returns `None`
/// when the request has no continuation array or does not serialize
/// (caller ends the loop).
/// Extracted from `handle_ccr_response` without behavior change.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_ccr_continuation(
    current_request: &mut serde_json::Value,
    items_field: &str,
    assistant_msg: serde_json::Value,
    tool_result_msg: serde_json::Value,
    provider: &str,
    config: &Config,
    request_id: &str,
    round: usize,
) -> Option<Vec<u8>> {
    // Build continuation messages: append assistant message + tool results.
    //
    // Some providers return sentinel-keyed shapes (a wrapper dict holding a
    // list of items) rather than a single message dict, because their turn
    // history is a flat item array rather than one role/content entry. When
    // we see such a sentinel, extend the continuation array with its list
    // instead of pushing the whole wrapper as one entry.
    if let Some(items) = current_request
        .get_mut(items_field)
        .and_then(|v| v.as_array_mut())
    {
        extend_or_push(items, assistant_msg, &["_openai_responses_output_items"]);
        extend_or_push(
            items,
            tool_result_msg,
            &["_openai_tool_results", "_openai_responses_tool_results"],
        );
    } else {
        tracing::warn!(
            request_id = %request_id,
            field = items_field,
            "ccr: no continuation array in request; cannot continue"
        );
        return None;
    }

    retail_continuation_breakpoint(current_request, provider, config, request_id, round + 1);

    // Re-send to upstream.
    match serde_json::to_vec(current_request) {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::warn!(
                request_id = %request_id,
                error = %e,
                "ccr: failed to serialize continuation request"
            );
            None
        }
    }
}

/// Unwrap a continuation send: transport/headers failures log the
/// triage line and end the loop. Returns `None` when the loop ends.
/// Extracted from `handle_ccr_response` without behavior change.
pub(super) fn unwrap_ccr_send(
    outcome: Result<reqwest::Response, Option<reqwest::Error>>,
    attempts: u32,
    continuation_started: &std::time::Instant,
    request_id: &str,
) -> Option<reqwest::Response> {
    match outcome {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(
                request_id = %request_id,
                attempts = attempts,
                error_kind = e.as_ref().map_or("headers_timeout", ccr_transport_kind),
                error_chain = %e.as_ref().map_or_else(
                    || format!(
                        "no response headers within {}s",
                        CCR_CONTINUATION_SEND_TIMEOUT.as_secs()
                    ),
                    ccr_error_chain
                ),
                upstream_ms = continuation_started.elapsed().as_millis() as u64,
                "ccr: upstream request failed during continuation"
            );
            None
        }
    }
}

/// Note the headers-received line, then refuse non-success continuations
/// with the refusal body as the diagnosis (a 4xx here means the body
/// this proxy built is wrong). Returns `None` when the loop ends.
/// Extracted from `handle_ccr_response` without behavior change.
pub(super) async fn check_ccr_response_status(
    resp: reqwest::Response,
    provider: &str,
    round: usize,
    attempts: u32,
    continuation_started: &std::time::Instant,
    request_id: &str,
) -> Option<reqwest::Response> {
    tracing::info!(
        request_id = %request_id,
        event = "ccr_continuation_upstream",
        provider = provider,
        round = round + 1,
        attempts = attempts,
        status = resp.status().as_u16(),
        // Time to response headers. What is left of the span up to
        // "retrieval handled" is reading and rewriting the stream.
        //
        // On a streamed continuation that is nearly all of it, so this
        // number dropped to around a second when Anthropic rounds started
        // streaming. Do not read it across that change as the model
        // getting faster: before, headers waited on the whole generation.
        upstream_ms = continuation_started.elapsed().as_millis() as u64,
        "ccr: continuation response headers received"
    );

    if resp.status().is_success() {
        return Some(resp);
    }
    // The status alone does not say which part of the request the
    // provider refused, and a 4xx here means the body this proxy built
    // is wrong — so the body of the refusal is the whole diagnosis.
    // The memory continuation below already logs it; this site did not,
    // which left every retrieval failure unreadable after the fact.
    let status = resp.status();
    let detail = resp.text().await.unwrap_or_default();
    tracing::warn!(
        request_id = %request_id,
        attempts = attempts,
        status = %status,
        detail = %detail.chars().take(2000).collect::<String>(),
        "ccr: upstream returned error during continuation"
    );
    None
}

/// Fresh send headers for one continuation attempt. Same Zen hygiene
/// as the CCR continuation above: fresh `x-opencode-request` per send,
/// no-op off zen routes.
/// Extracted from `send_ccr_continuation` without behavior change.
fn fresh_ccr_send_headers(
    outgoing_headers: &http::HeaderMap,
    attempt: u32,
    round: usize,
    request_id: &str,
) -> http::HeaderMap {
    // Zen free-tier hygiene: the forward path's header map carries
    // the original request's `x-opencode-request` UUID; the real
    // CLI mints one per POST, so refresh before every continuation
    // send (no-op off zen routes). See `refresh_zen_request_id`.
    let mut continuation_headers = outgoing_headers.clone();
    if crate::routed::quirks::refresh_zen_request_id(&mut continuation_headers) {
        tracing::debug!(
            request_id = %request_id,
            attempt = attempt,
            round = round + 1,
            "ccr: refreshed x-opencode-request for continuation send"
        );
    }
    continuation_headers
}

/// Await one continuation send under the bounded headers wait:
/// `.send()` resolves at response headers, so a stall here is
/// transport, never a slow model (body streams after, under the
/// total timeout). `None` is that stall, logged once here.
/// Extracted from `send_ccr_continuation` without behavior change.
async fn await_ccr_headers(
    client: &reqwest::Client,
    upstream_url: &url::Url,
    continuation_headers: &http::HeaderMap,
    body: Vec<u8>,
    continuation_body_len: usize,
    attempt: u32,
    request_id: &str,
) -> Result<reqwest::Response, Option<reqwest::Error>> {
    // Bounded headers wait: `.send()` resolves at response headers,
    // so a stall here is transport, never a slow model (body streams
    // after, under the total timeout). Without this the 600s client
    // timeout is the only bound and a hung attempt sits for tens of
    // seconds before the retry can help. `None` is that stall, logged
    // once here; the shared retry/backoff below treats it like any
    // other retryable transport failure.
    match tokio::time::timeout(
        CCR_CONTINUATION_SEND_TIMEOUT,
        client
            .post(upstream_url.clone())
            .headers(crate::headers::headers_for_json_body(
                continuation_headers,
                &body,
            ))
            .body(body)
            .send(),
    )
    .await
    {
        Ok(r) => r.map_err(Some),
        Err(_) => {
            tracing::warn!(
                request_id = %request_id,
                attempt = attempt,
                timeout_secs = CCR_CONTINUATION_SEND_TIMEOUT.as_secs(),
                continuation_body_bytes = continuation_body_len,
                "ccr: continuation timed out waiting for response headers; retrying"
            );
            Err(None)
        }
    }
}

/// What one CCR continuation send decided: the response (or transport
/// stall marker), plus the attempt count for the triage logs.
/// Extracted from `handle_ccr_response` without behavior change.
pub(super) struct CcrSendOutcome {
    pub outcome: Result<reqwest::Response, Option<reqwest::Error>>,
    pub attempts: u32,
}

/// Send one CCR continuation with bounded headers-wait and retry: a
/// stall here is transport (`.send()` resolves at response headers,
/// never a slow model), overload and transport blips retry with
/// backoff, a 4xx is the request itself being wrong. A continuation
/// that fails is not a soft outcome: the retrieval is already parsed
/// and the content already fetched, and giving up leaves the model
/// with an unanswered `headroom_retrieve`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn send_ccr_continuation(
    client: &reqwest::Client,
    upstream_url: &url::Url,
    outgoing_headers: &http::HeaderMap,
    continuation_body: Vec<u8>,
    results: &[CcrToolResult],
    max_rounds: usize,
    base: Option<(
        &crate::cache_stabilization::drift_detector::StructuralHash,
        crate::cache_stabilization::drift_detector::ApiKind,
    )>,
    current_request: &serde_json::Value,
    request_id: &str,
    round: usize,
) -> CcrSendOutcome {
    // Shape facts for timeout triage (c): three header-timeouts with
    // zero bytes smell like a malformed or enormous rebuilt body rather
    // than a network blip, and the old log could not distinguish those.
    // `results_chars` bounds the fetched-content contribution; the
    // request JSON itself is measured serialized below.
    tracing::info!(
        request_id = %request_id,
        round = round + 1,
        max_rounds = max_rounds,
        results_count = results.len(),
        results_chars = results.iter().map(|r| r.content.len()).sum::<usize>(),
        "ccr: sending continuation request"
    );

    if let Some((base, kind)) = base {
        cache_stabilization::drift_detector::check_continuation_prefix(
            base,
            current_request,
            kind,
            request_id,
            round + 1,
        );
    }

    // A continuation that fails is not a soft outcome: the retrieval is
    // already parsed and the content already fetched, and giving up here
    // leaves the model with an unanswered `headroom_retrieve`.
    let mut attempt = 0;
    let outcome = loop {
        let continuation_headers =
            fresh_ccr_send_headers(outgoing_headers, attempt, round, request_id);
        let outcome = await_ccr_headers(
            client,
            upstream_url,
            &continuation_headers,
            continuation_body.clone(),
            continuation_body.len(),
            attempt,
            request_id,
        )
        .await;
        let retryable = match &outcome {
            Err(_) => true,
            Ok(r) => {
                let s = r.status();
                s.is_server_error() || s == reqwest::StatusCode::TOO_MANY_REQUESTS
            }
        };
        if !retryable || attempt >= CCR_CONTINUATION_RETRIES {
            break outcome;
        }
        attempt += 1;
        let backoff = std::time::Duration::from_millis(250 << (attempt - 1));
        note_ccr_retry(&outcome, attempt, backoff, request_id);
        crate::observability::ccr_retrieval::observe_continuation_retry();
        tokio::time::sleep(backoff).await;
    };
    CcrSendOutcome {
        outcome,
        attempts: attempt + 1,
    }
}

/// Log one CCR continuation retry at the warn level that names the
/// failure class: transport kind + chain, or the upstream status.
fn note_ccr_retry(
    outcome: &Result<reqwest::Response, Option<reqwest::Error>>,
    attempt: u32,
    backoff: std::time::Duration,
    request_id: &str,
) {
    match outcome {
        Err(Some(e)) => tracing::warn!(
            request_id = %request_id,
            error_kind = ccr_transport_kind(e),
            error_chain = %ccr_error_chain(e),
            attempt = attempt,
            backoff_ms = backoff.as_millis() as u64,
            "ccr: continuation transport error; retrying"
        ),
        // Headers-wait stall: already logged above with its timeout;
        // nothing more to say, just back off and resend.
        Err(None) => {}
        Ok(r) => tracing::warn!(
            request_id = %request_id,
            status = %r.status(),
            attempt = attempt,
            backoff_ms = backoff.as_millis() as u64,
            "ccr: continuation rejected upstream; retrying"
        ),
    }
}

/// What one CCR round-body read decided: the next turn, a same-round
/// retry, or the end of the loop.
/// Extracted from `handle_ccr_response` without behavior change.
pub(super) enum CcrRoundRead {
    Advance(serde_json::Value),
    Retry,
    Done,
}

/// Body stall / transport cut after 200 headers: the same class as a
/// terminal-less stream, and the send loop only retries the headers
/// wait. Shares the per-turn cut budget; exhaustion falls through to
/// fallback (a) as before.
/// Extracted from `read_ccr_round_body` without behavior change.
async fn note_ccr_body_stall(
    error: String,
    cut_attempts: &mut u32,
    request_id: &str,
) -> CcrRoundRead {
    if *cut_attempts < CCR_CONTINUATION_RETRIES {
        *cut_attempts += 1;
        crate::observability::ccr_retrieval::observe_continuation_retry();
        tracing::warn!(
            request_id = %request_id,
            error = %error,
            cut_attempt = *cut_attempts,
            backoff_ms = 250u64 << (*cut_attempts - 1),
            "ccr: failed to read continuation response body; retrying same round"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250 << (*cut_attempts - 1))).await;
        return CcrRoundRead::Retry;
    }
    tracing::warn!(
        request_id = %request_id,
        error = %error,
        "ccr: failed to read continuation response body"
    );
    CcrRoundRead::Done
}

/// Fold failure: retryable cut (terminal-less SSE or truncated JSON)
/// resends the identical deterministic re-send; explicit verdicts and
/// deterministic garbage fall through to fallback (a).
/// Extracted from `read_ccr_round_body` without behavior change.
async fn retry_or_fail_ccr_fold(
    bytes: bytes::Bytes,
    content_type: &str,
    provider: &str,
    cut_attempts: &mut u32,
    request_id: &str,
) -> CcrRoundRead {
    let terminal = continuation_stream_terminal(&bytes, provider);
    // Retryable cut, per provider fold (see
    // continuation_cut_retryable): a cut stream
    // resends the identical deterministic re-send;
    // explicit verdicts and deterministic garbage
    // fall through to fallback (a) below.
    if continuation_cut_retryable(&bytes, content_type, provider)
        && *cut_attempts < CCR_CONTINUATION_RETRIES
    {
        // Same round, unchanged request: the
        // tool_result is deterministic, only the
        // transport failed. `rounds` is not consumed
        // and `last_fetched` is recomputed below.
        *cut_attempts += 1;
        crate::observability::ccr_retrieval::observe_continuation_retry();
        tracing::warn!(
            request_id = %request_id,
            body_bytes = bytes.len(),
            cut_attempt = *cut_attempts,
            backoff_ms = 250u64 << (*cut_attempts - 1),
            "ccr: continuation stream ended with no terminal event; retrying same round"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250 << (*cut_attempts - 1))).await;
        return CcrRoundRead::Retry;
    }
    tracing::warn!(
        request_id = %request_id,
        body_bytes = bytes.len(),
        content_type = %content_type,
        terminal = terminal.unwrap_or("absent"),
        body_head = %String::from_utf8_lossy(
            &bytes[..bytes.len().min(200)]
        ),
        "ccr: failed to parse continuation response"
    );
    CcrRoundRead::Done
}

/// Read one CCR continuation body and fold it back into a turn: retry
/// the identical deterministic re-send on a retryable cut (terminal-
/// less SSE or truncated JSON, bounded per turn) or a body stall after
/// 200 headers; anything explicit falls through to fallback (a).
pub(super) async fn read_ccr_round_body(
    resp: reqwest::Response,
    provider: &str,
    cut_attempts: &mut u32,
    round_usage: &mut CcrRoundUsage,
    current_response: &serde_json::Value,
    request_id: &str,
) -> CcrRoundRead {
    // `bytes()` consumes the response; snapshot what the fold needs first.
    let content_type = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = match read_continuation_body(resp).await {
        Ok(bytes) => bytes,
        Err(e) => return note_ccr_body_stall(e, cut_attempts, request_id).await,
    };
    // The response about to be dropped was still billed — including a
    // cut body that then retries the same round.
    round_usage.add_response(current_response);
    match continuation_turn_from_body(&bytes, Some(&content_type), provider) {
        Some(next) => CcrRoundRead::Advance(next),
        None => {
            retry_or_fail_ccr_fold(bytes, &content_type, provider, cut_attempts, request_id).await
        }
    }
}

/// Build one fallback note: store-fetched content (capped) when the
/// lookup already succeeded this round, else the failure text.
/// Extracted from `handle_ccr_response` without behavior change.
fn fallback_note_for_call(
    call: &CcrToolCall,
    last_fetched: &[CcrToolResult],
    request_id: &str,
) -> CcrToolResult {
    // Fallback (a): the store lookup already succeeded this round —
    // `results` holds the fetched content — and only the upstream
    // re-send died. Serve what we hold instead of a failure text:
    // the turn resolves instead of stalling on an unanswered call.
    // Capped: an unbounded splice just moves the blowup client-side.
    const CCR_FALLBACK_MAX_CHARS: usize = 24_000;
    let fallback = last_fetched
        .iter()
        .find(|r| r.tool_call_id == call.tool_call_id && r.success && !r.content.is_empty());
    match fallback {
        Some(hit) => {
            let truncated = hit.content.chars().count() > CCR_FALLBACK_MAX_CHARS;
            let content = if truncated {
                let kept: String = hit.content.chars().take(CCR_FALLBACK_MAX_CHARS).collect();
                format!(
                    "{kept}\n\n[truncated: retrieved content exceeded \
                     {CCR_FALLBACK_MAX_CHARS} characters]"
                )
            } else {
                hit.content.clone()
            };
            tracing::info!(
                request_id = %request_id,
                tool_call_id = %call.tool_call_id,
                truncated,
                "ccr: serving store-fetched content after continuation failure",
            );
            headroom_core::ccr::response_handler::CcrToolResult {
                tool_call_id: call.tool_call_id.clone(),
                content,
                success: true,
                items_retrieved: hit.items_retrieved,
            }
        }
        None => headroom_core::ccr::response_handler::CcrToolResult {
            tool_call_id: call.tool_call_id.clone(),
            content: "The proxy could not complete a context retrieval for this turn.".to_string(),
            success: false,
            items_retrieved: 0,
        },
    }
}

/// Classify the loop outcome before returning. Only claim success when
/// no `headroom_retrieve` remains; a retrieve left standing is a real
/// failure. Whatever is left cannot be answered: out of rounds,
/// upstream refused every attempt, or a provider shape this path
/// cannot splice. Do not hand it back as a `tool_use` — the client
/// never declared `headroom_retrieve` and cannot resolve it, so the
/// turn ends on a call nothing will ever answer. Say so in the turn
/// instead, which is what the streamed path already does.
/// Extracted from `handle_ccr_response` without behavior change.
pub(super) fn resolve_ccr_residual(
    handler: &headroom_core::ccr::response_handler::CCRResponseHandler,
    current_response: &mut serde_json::Value,
    provider: &str,
    last_fetched: &[CcrToolResult],
    request_id: &str,
) {
    use headroom_core::ccr::response_handler::RESIDUAL_CCR_RESOLVED;

    // Classify the outcome before returning. Only claim success when no
    // `headroom_retrieve` remains; a retrieve left standing is a real failure.
    match handler.residual_ccr_status(current_response, provider) {
        RESIDUAL_CCR_RESOLVED => {
            tracing::info!(request_id = %request_id, "ccr: retrieval handled successfully");
            crate::observability::ccr_retrieval::observe_outcome(
                crate::observability::ccr_retrieval::OUTCOME_CONTINUATION,
                1,
            );
        }
        status => {
            // Whatever is left cannot be answered: out of rounds, upstream
            // refused every attempt, or a provider shape this path cannot
            // splice. Do not hand it back as a `tool_use` — the client never
            // declared `headroom_retrieve` and cannot resolve it, so the turn
            // ends on a call nothing will ever answer. Say so in the turn
            // instead, which is what the streamed path already does.
            let (residual, _) = handler.parse_ccr_tool_calls(current_response, provider);
            let notes: Vec<_> = residual
                .iter()
                .map(|call| fallback_note_for_call(call, last_fetched, request_id))
                .collect();
            let spliced = handler.splice_ccr_results_as_text(current_response, &notes, provider);
            tracing::warn!(
                request_id = %request_id,
                status = %status,
                residual = residual.len(),
                spliced = spliced,
                "ccr: headroom_retrieve remains unresolved with no client tool call"
            );
            crate::observability::ccr_retrieval::observe_outcome(
                crate::observability::ccr_retrieval::OUTCOME_UNRESOLVED,
                residual.len().max(1) as u64,
            );
        }
    }
}
