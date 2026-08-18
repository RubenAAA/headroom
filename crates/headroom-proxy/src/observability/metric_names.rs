//! Centralised metric-name + label-key constants — Phase G PR-G3.
//!
//! Realignment build-constraint "configurable: every metric name + label
//! vocabulary defined in one place" applies here. The Bedrock D3
//! metrics in [`super::prometheus`] predate this module; they keep
//! their inline literals (a churn-cost decision documented in the
//! PR-G3 commit) but every PR-G3 metric and its labels live here.
//!
//! # Naming convention
//!
//! - `METRIC_*` — the wire-name string. Prometheus convention:
//!   `_total` for counters, `_seconds` / no suffix for histograms.
//! - `METRIC_*_HELP` — the HELP-line text used at registration. Kept
//!   alongside the wire name so a rename catches both in one diff.
//! - `LABEL_*` — the label-key string. Reuse across metrics where
//!   the dimension is the same (`provider`, `strategy`, …).

// ---------- proxy_cache_hit_rate_per_session ----------

pub const METRIC_PROXY_CACHE_HIT_RATE_PER_SESSION: &str = "proxy_cache_hit_rate_per_session";
pub const METRIC_PROXY_CACHE_HIT_RATE_PER_SESSION_HELP: &str =
    "Per-session cache hit rate observed at the Rust proxy from \
     usage.cache_read_input_tokens / (input + cache_read + cache_creation). \
     Phase H canary gate: parity with the Python proxy baseline.";

// ---------- proxy_cache_recache_events_total ----------

pub const METRIC_PROXY_CACHE_RECACHE_EVENTS_TOTAL: &str = "proxy_cache_recache_events_total";
pub const METRIC_PROXY_CACHE_RECACHE_EVENTS_TOTAL_HELP: &str =
    "CTX-7: count of re-cache events — turns where usage showed the \
     prompt-cache prefix was re-written inside the TTL window instead \
     of read back. Labelled by reason (drift axis from PR-E6, or \
     'unknown' when the drift detector saw stable bytes).";

// ---------- proxy_cache_recache_wasted_tokens_total ----------

pub const METRIC_PROXY_CACHE_RECACHE_WASTED_TOKENS_TOTAL: &str =
    "proxy_cache_recache_wasted_tokens_total";
pub const METRIC_PROXY_CACHE_RECACHE_WASTED_TOKENS_TOTAL_HELP: &str =
    "CTX-7: cumulative billed tokens wasted re-writing prompt-cache \
     prefixes that should have been cache reads (summed wasted_tokens \
     across recache events).";

// ---------- proxy_cache_replay_alternates_evicted_total ----------

pub const METRIC_PROXY_CACHE_REPLAY_ALTERNATES_EVICTED_TOTAL: &str =
    "proxy_cache_replay_alternates_evicted_total";
pub const METRIC_PROXY_CACHE_REPLAY_ALTERNATES_EVICTED_TOTAL_HELP: &str =
    "Count of stored branch prefixes dropped from a session's alternates \
     because the count or message budget was full. One session key carries \
     several interleaved streams, and each holds the prefix its own next turn \
     replays; an evicted stream busts instead. Deep conversations spend the \
     message budget fastest, so this is where a raised cap would pay.";

// ---------- proxy_compression_ratio_by_strategy ----------

pub const METRIC_PROXY_COMPRESSION_RATIO_BY_STRATEGY: &str = "proxy_compression_ratio_by_strategy";
pub const METRIC_PROXY_COMPRESSION_RATIO_BY_STRATEGY_HELP: &str =
    "Compression ratio (compressed_tokens / original_tokens) observed \
     per block that was actually shrunk by the live-zone dispatcher. \
     Labelled by strategy (smart_crusher/log_compressor/…) and \
     detected content_type.";

// ---------- proxy_tokens_saved_total ----------

pub const METRIC_PROXY_TOKENS_SAVED_TOTAL: &str = "proxy_tokens_saved_total";
pub const METRIC_PROXY_TOKENS_SAVED_TOTAL_HELP: &str =
    "Cumulative input tokens removed from the wire by the live-zone \
     dispatcher (original_tokens - compressed_tokens, summed per shrunk \
     block). The running 'you saved X tokens' total the per-block \
     ratio histogram can't express. Labelled by strategy + content_type.";

// ---------- proxy_compression_rejected_by_token_check_total ----------

pub const METRIC_PROXY_COMPRESSION_REJECTED_BY_TOKEN_CHECK_TOTAL: &str =
    "proxy_compression_rejected_by_token_check_total";
pub const METRIC_PROXY_COMPRESSION_REJECTED_BY_TOKEN_CHECK_TOTAL_HELP: &str =
    "Count of compressor runs whose output failed the tokenizer-validated \
     shrink check (compressed_tokens >= original_tokens). Surfaces 'we ran \
     but kept the original' cases that would otherwise be invisible.";

// ---------- proxy_compression_declined_no_shrink_total ----------

pub const METRIC_PROXY_COMPRESSION_DECLINED_NO_SHRINK_TOTAL: &str =
    "proxy_compression_declined_no_shrink_total";
pub const METRIC_PROXY_COMPRESSION_DECLINED_NO_SHRINK_TOTAL_HELP: &str =
    "Count of compressor runs the dispatcher declined because the output was \
     not smaller in bytes than the input. These used to travel to the \
     tokenizer and land in proxy_compression_rejected_by_token_check_total; \
     counting them here keeps that visible. Read alongside \
     proxy_compression_ratio_by_strategy_sum: this rising while accepted \
     compression holds steady means the gate is absorbing waste, both \
     falling together means it is declining work that pays.";

// ---------- proxy_passthrough_bytes_modified_total ----------

pub const METRIC_PROXY_PASSTHROUGH_BYTES_MODIFIED_TOTAL: &str =
    "proxy_passthrough_bytes_modified_total";
pub const METRIC_PROXY_PASSTHROUGH_BYTES_MODIFIED_TOTAL_HELP: &str =
    "Bytes modified on a path that is supposed to passthrough verbatim. \
     MUST stay 0 outside the compression-on hot path. Any non-zero rate \
     fires the cache-safety alarm.";

// ---------- proxy_rate_limit_remaining_* ----------

pub const METRIC_PROXY_RATE_LIMIT_REMAINING_REQUESTS: &str = "proxy_rate_limit_remaining_requests";
pub const METRIC_PROXY_RATE_LIMIT_REMAINING_REQUESTS_HELP: &str =
    "Upstream-reported remaining requests for the current window, \
     extracted from rate-limit response headers (anthropic-ratelimit-* \
     or x-ratelimit-*). Per-provider, per-window-bucket gauge.";

pub const METRIC_PROXY_RATE_LIMIT_REMAINING_TOKENS: &str = "proxy_rate_limit_remaining_tokens";
pub const METRIC_PROXY_RATE_LIMIT_REMAINING_TOKENS_HELP: &str =
    "Upstream-reported remaining tokens for the current window, extracted \
     from rate-limit response headers (anthropic-ratelimit-*-tokens or \
     x-ratelimit-remaining-tokens).";

pub const METRIC_PROXY_RATE_LIMIT_REMAINING_INPUT_TOKENS: &str =
    "proxy_rate_limit_remaining_input_tokens";
pub const METRIC_PROXY_RATE_LIMIT_REMAINING_INPUT_TOKENS_HELP: &str =
    "Upstream-reported remaining INPUT tokens for the current window. \
     Anthropic separates input and output token budgets in its \
     ratelimit headers; this gauge tracks the input bucket.";

pub const METRIC_PROXY_RATE_LIMIT_REMAINING_OUTPUT_TOKENS: &str =
    "proxy_rate_limit_remaining_output_tokens";
pub const METRIC_PROXY_RATE_LIMIT_REMAINING_OUTPUT_TOKENS_HELP: &str =
    "Upstream-reported remaining OUTPUT tokens for the current window. \
     Anthropic-only header on present providers.";

// ---------- proxy_ratelimit_unified_* (subscription / OAuth) ----------
//
// API-key traffic exposes the `*-remaining` family above. Claude
// subscription / OAuth traffic instead returns
// `anthropic-ratelimit-unified-*` (per-window utilization + status +
// reset, plus overage / fallback). `utilization` is the consumed
// fraction [0,1] of a window, so `1 - utilization` is the remaining
// subscription headroom — the number a subscription operator wants.

pub const METRIC_PROXY_RATELIMIT_UNIFIED_UTILIZATION: &str = "proxy_ratelimit_unified_utilization";
pub const METRIC_PROXY_RATELIMIT_UNIFIED_UTILIZATION_HELP: &str =
    "Consumed fraction [0,1] of a Claude-subscription rate-limit window, \
     from anthropic-ratelimit-unified-<window>-utilization. `1 - value` \
     is the remaining headroom. Labelled by window (5h, 7d, per-model \
     like 7d_sonnet).";

pub const METRIC_PROXY_RATELIMIT_UNIFIED_RESET_SECONDS: &str =
    "proxy_ratelimit_unified_reset_seconds";
pub const METRIC_PROXY_RATELIMIT_UNIFIED_RESET_SECONDS_HELP: &str =
    "Unix epoch (seconds) at which a subscription rate-limit window \
     resets, from anthropic-ratelimit-unified-<window>-reset. Window \
     `overall` carries the top-level anthropic-ratelimit-unified-reset.";

pub const METRIC_PROXY_RATELIMIT_UNIFIED_THROTTLED: &str = "proxy_ratelimit_unified_throttled";
pub const METRIC_PROXY_RATELIMIT_UNIFIED_THROTTLED_HELP: &str =
    "1 when a subscription window's status is anything other than \
     'allowed' (rejected/queueing/blocked), else 0. Boolean alarm \
     signal per window; the full status string is on the structured \
     log line paired with each update. Window `overage` carries \
     anthropic-ratelimit-unified-overage-status.";

pub const METRIC_PROXY_RATELIMIT_UNIFIED_FALLBACK_PERCENTAGE: &str =
    "proxy_ratelimit_unified_fallback_percentage";
pub const METRIC_PROXY_RATELIMIT_UNIFIED_FALLBACK_PERCENTAGE_HELP: &str =
    "anthropic-ratelimit-unified-fallback-percentage [0,1]: the share \
     of traffic the upstream is steering to fallback capacity. \
     Top-level (no window label).";

// ---------- proxy_service_tier_count_total ----------

pub const METRIC_PROXY_SERVICE_TIER_COUNT_TOTAL: &str = "proxy_service_tier_count_total";
pub const METRIC_PROXY_SERVICE_TIER_COUNT_TOTAL_HELP: &str =
    "Count of requests/responses observed at the proxy, labelled by the \
     OpenAI Responses service_tier the request resolved into (auto, \
     default, flex, on_demand, priority).";

// ---------- proxy_response_status_count_total ----------

pub const METRIC_PROXY_RESPONSE_STATUS_COUNT_TOTAL: &str = "proxy_response_status_count_total";
pub const METRIC_PROXY_RESPONSE_STATUS_COUNT_TOTAL_HELP: &str =
    "Count of OpenAI Responses outcomes labelled by terminal status \
     (completed, incomplete, failed, cancelled, in_progress). \
     'incomplete' detail lands in the structured log paired with each \
     increment.";

// Phase G PR-G3 remediation (C3 + C4): the metric-name constants
// for `proxy_image_generation_call_log_redacted_total`,
// `wrap_rtk_invocations_total`, and `wrap_rtk_tokens_saved_per_session`
// were removed because the underlying counters had no production
// emit site on the Rust side. Image redaction is exported by the
// Python proxy (`headroom/proxy/prometheus_metrics.py`), its natural
// owner. The two `wrap_rtk_*` names are gone for good: the rtk
// integration they measured has been removed from Headroom.
// See `docs/observability.md`.

// ---------- proxy_upstream_retries_total ----------

pub const METRIC_PROXY_UPSTREAM_RETRIES_TOTAL: &str = "proxy_upstream_retries_total";
pub const METRIC_PROXY_UPSTREAM_RETRIES_TOTAL_HELP: &str =
    "Count of upstream requests re-sent after a transient failure, labelled \
     by forward path (anthropic, local_model) and reason (status_429, \
     status_529, status_5xx, transport). One increment per re-send, so a \
     request that succeeds on its third try contributes 2. Retries cost \
     latency and re-bill the input tokens, and until this counter existed \
     they were visible only in the logs.";

// ---------- ctx_proactive_expansion_bytes_total ----------

pub const METRIC_CTX_PROACTIVE_EXPANSION_BYTES_TOTAL: &str = "ctx_proactive_expansion_bytes_total";
pub const METRIC_CTX_PROACTIVE_EXPANSION_BYTES_TOTAL_HELP: &str =
    "CCR Phase 4: cumulative bytes of previously-offloaded content appended \
     back into the latest user turn by proactive expansion. This is the \
     counterweight to ctx_offloaded_bytes_total — offload removes bytes, \
     expansion puts them back, and only the difference is a real saving.";

// ---------- ctx_proactive_expansion_cache_write_tokens_total ----------

pub const METRIC_CTX_PROACTIVE_EXPANSION_CACHE_WRITE_TOKENS_TOTAL: &str =
    "ctx_proactive_expansion_cache_write_tokens_total";
pub const METRIC_CTX_PROACTIVE_EXPANSION_CACHE_WRITE_TOKENS_TOTAL_HELP: &str =
    "CCR Phase 4: Anthropic cache-creation input tokens on requests that \
     injected proactive expansion. Provider usage cannot split the expansion \
     from the rest of that newly-written cache segment, so this records the \
     actual write charged to the affected request rather than estimating from \
     expansion bytes.";

// ---------- ctx_proactive_expansions_total ----------

pub const METRIC_CTX_PROACTIVE_EXPANSIONS_TOTAL: &str = "ctx_proactive_expansions_total";
pub const METRIC_CTX_PROACTIVE_EXPANSIONS_TOTAL_HELP: &str =
    "CCR Phase 4: count of requests that had at least one previously-offloaded \
     block re-inserted into the latest user turn.";

// ---------- proxy_stream_incomplete_total ----------

pub const METRIC_PROXY_STREAM_INCOMPLETE_TOTAL: &str = "proxy_stream_incomplete_total";
pub const METRIC_PROXY_STREAM_INCOMPLETE_TOTAL_HELP: &str =
    "Count of SSE streams that ended without their terminal event \
     (Anthropic `message_stop`), labelled by provider. Usage totals arrive \
     with that event, so these turns are dropped from the cost and savings \
     books rather than booked at their partial figures. A rising rate means \
     the books are getting less complete, not that nothing happened.";

// ---------- ctx_offloaded_bytes_total ----------

pub const METRIC_CTX_OFFLOADED_BYTES_TOTAL: &str = "ctx_offloaded_bytes_total";
pub const METRIC_CTX_OFFLOADED_BYTES_TOTAL_HELP: &str =
    "CTX-5/6: cumulative bytes offloaded from tool_result blocks into the \
     CCR store. Incremented on the request path when ctx_offload replaces \
     a block with a digest.";

// ---------- ctx_offloaded_blocks_total ----------

pub const METRIC_CTX_OFFLOADED_BLOCKS_TOTAL: &str = "ctx_offloaded_blocks_total";
pub const METRIC_CTX_OFFLOADED_BLOCKS_TOTAL_HELP: &str =
    "CTX-5/6: count of tool_result blocks offloaded (replaced with a \
     deterministic digest) across all requests.";

// ---------- ctx_recall_injections_total ----------

pub const METRIC_CTX_RECALL_INJECTIONS_TOTAL: &str = "ctx_recall_injections_total";
pub const METRIC_CTX_RECALL_INJECTIONS_TOTAL_HELP: &str =
    "CTX-5/6: count of recall/resume blocks injected into the first user \
     message of a conversation (CTX-4 injection engine).";

// ---------- ctx_injection_clipped_bytes_total ----------

pub const METRIC_CTX_INJECTION_CLIPPED_BYTES_TOTAL: &str = "ctx_injection_clipped_bytes_total";
pub const METRIC_CTX_INJECTION_CLIPPED_BYTES_TOTAL_HELP: &str =
    "Bytes dropped by the shared per-request injection budget, labelled by \
     stage (proactive_expansion / recall / memory). Non-zero means a stage \
     wanted more room than --max-injection-bytes allowed.";

// ---------- ctx_retrieval_hits_total ----------

pub const METRIC_CTX_RETRIEVAL_HITS_TOTAL: &str = "ctx_retrieval_hits_total";
pub const METRIC_CTX_RETRIEVAL_HITS_TOTAL_HELP: &str =
    "PR-J5: count of /ctx/get retrievals that found the offloaded original \
     in the CCR store.";

// ---------- ctx_retrieval_misses_total ----------

pub const METRIC_CTX_RETRIEVAL_MISSES_TOTAL: &str = "ctx_retrieval_misses_total";
pub const METRIC_CTX_RETRIEVAL_MISSES_TOTAL_HELP: &str =
    "PR-J5: count of /ctx/get retrievals for a hash absent from the CCR \
     store (expired, evicted, or never offloaded). A rising rate flags an \
     information-loss risk — see REALIGNMENT/13 §8.";

// ---------- ctx_search_queries_total ----------

pub const METRIC_CTX_SEARCH_QUERIES_TOTAL: &str = "ctx_search_queries_total";
pub const METRIC_CTX_SEARCH_QUERIES_TOTAL_HELP: &str =
    "CTX-5: count of search queries served via the /ctx/search endpoint.";

// ---------- proxy_upstream_responses_total ----------

pub const METRIC_PROXY_UPSTREAM_RESPONSES_TOTAL: &str = "proxy_upstream_responses_total";
pub const METRIC_PROXY_UPSTREAM_RESPONSES_TOTAL_HELP: &str =
    "Responses received from upstream, of any status. The denominator \
     for the rejection rate — a refusal count on its own says nothing \
     about whether the proxy is healthy.";

// ---------- proxy_upstream_rejections_total ----------

pub const METRIC_PROXY_UPSTREAM_REJECTIONS_TOTAL: &str = "proxy_upstream_rejections_total";
pub const METRIC_PROXY_UPSTREAM_REJECTIONS_TOTAL_HELP: &str =
    "Non-2xx responses from upstream, labelled by HTTP status. A \
     rejected turn is work lost and a cached prefix rewritten, so this \
     is costlier than any cache miss. 429 is kept separate by its label \
     because rate limiting is the provider throttling a healthy proxy.";

// ---------- proxy_ccr_splice_dropped_blocks_total ----------

pub const METRIC_PROXY_CCR_SPLICE_DROPPED_BLOCKS_TOTAL: &str =
    "proxy_ccr_splice_dropped_blocks_total";
pub const METRIC_PROXY_CCR_SPLICE_DROPPED_BLOCKS_TOTAL_HELP: &str =
    "Content blocks the CCR retrieval splice refused to forward to the \
     client, labelled by reason. `unresolved_proxy_tool` is routine; \
     `continuation_thinking` and `already_streamed` are the two shapes \
     that made upstream refuse the *following* turn, so a non-zero \
     count next to a rising rejection rate names the cause.";

// ---------- proxy_ccr_retrieval_outcomes_total ----------

pub const METRIC_PROXY_CCR_RETRIEVAL_OUTCOMES_TOTAL: &str = "proxy_ccr_retrieval_outcomes_total";
pub const METRIC_PROXY_CCR_RETRIEVAL_OUTCOMES_TOTAL_HELP: &str =
    "How each buffered `headroom_retrieve` ended, labelled by outcome. \
     `continuation` is the normal path: a second upstream call carried the \
     content back as a tool_result. `spliced_mixed` is a turn that also held \
     a real client tool call, so the content went in as text instead — that \
     turn used to lose the retrieval outright. `unresolved` means the model \
     asked and got nothing, and should stay at zero.";

// ---------- proxy_ccr_continuation_retries_total ----------

pub const METRIC_PROXY_CCR_CONTINUATION_RETRIES_TOTAL: &str =
    "proxy_ccr_continuation_retries_total";
pub const METRIC_PROXY_CCR_CONTINUATION_RETRIES_TOTAL_HELP: &str =
    "Continuation POSTs re-sent after a transport error, a 5xx, or a 429. \
     Read it against `proxy_ccr_retrieval_outcomes_total`: retries rising \
     while `unresolved` stays at zero means the backoff is doing its job.";

// ---------- proxy_cache_tail_breakpoint_total ----------

pub const METRIC_PROXY_CACHE_BREAKPOINT_SPREAD_TOTAL: &str =
    "proxy_cache_tail_breakpoint_total";
pub const METRIC_PROXY_CACHE_BREAKPOINT_SPREAD_TOTAL_HELP: &str =
    "Anthropic requests the tail-breakpoint stage looked at, labelled by \
     whether it moved the message marker. Most requests need no move and the \
     refusals are silent, so `applied` against `skipped` is the only way to \
     tell the stage is working from the stage never firing.";

// ---------- shared label keys ----------

pub const LABEL_PROVIDER: &str = "provider";
pub const LABEL_STRATEGY: &str = "strategy";
pub const LABEL_CONTENT_TYPE: &str = "content_type";
pub const LABEL_PATH: &str = "path";
pub const LABEL_TIER: &str = "tier";
pub const LABEL_STATUS: &str = "status";
pub const LABEL_WINDOW: &str = "window";
pub const LABEL_REASON: &str = "reason";
pub const LABEL_OUTCOME: &str = "outcome";

// ---------- bounded label vocabularies ----------

/// OpenAI service-tier values per the Responses API spec
/// (`service_tier` field on the response object). The metric label
/// vocabulary is **strictly** this set plus a `"scale"` value
/// (documented in OpenAI's tier-pricing page) and a sentinel
/// `"other"` bucket for anything else, so a malicious client posting
/// `{"service_tier":"<random>"}` per request cannot blow up
/// cardinality.
pub mod service_tier {
    pub const AUTO: &str = "auto";
    pub const DEFAULT: &str = "default";
    pub const FLEX: &str = "flex";
    pub const ON_DEMAND: &str = "on_demand";
    pub const PRIORITY: &str = "priority";
    pub const SCALE: &str = "scale";
    /// Sentinel for any unknown / unrecognised tier value. Prevents
    /// label-cardinality DoS from arbitrary inbound JSON.
    pub const OTHER: &str = "other";

    /// Validate an inbound `service_tier` string against the bounded
    /// vocabulary. Returns the matching `&'static` constant or
    /// [`OTHER`] for any unrecognised value (with a tracing::warn so
    /// wire-format drift is loud rather than silently bucketed).
    ///
    /// The matching is case-sensitive — the OpenAI spec is
    /// case-sensitive on these strings; a case-different value is
    /// treated as drift, not as the same tier.
    pub fn validate(raw: &str) -> &'static str {
        match raw {
            AUTO => AUTO,
            DEFAULT => DEFAULT,
            FLEX => FLEX,
            ON_DEMAND => ON_DEMAND,
            PRIORITY => PRIORITY,
            SCALE => SCALE,
            _ => {
                tracing::warn!(
                    event = "service_tier_unknown",
                    raw = %raw,
                    bucket = OTHER,
                    "unknown service_tier value bucketed to 'other' to bound cardinality"
                );
                OTHER
            }
        }
    }
}

/// OpenAI Responses terminal-status vocabulary. `in_progress` is the
/// non-terminal entry — included so observers see a request that
/// closed mid-stream (we increment on the last status seen).
pub mod response_status {
    pub const COMPLETED: &str = "completed";
    pub const INCOMPLETE: &str = "incomplete";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
    pub const IN_PROGRESS: &str = "in_progress";
}
