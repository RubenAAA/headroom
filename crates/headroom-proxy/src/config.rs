//! Configuration for the proxy: CLI flags + env vars.

use clap::{Parser, ValueEnum};
use headroom_core::rollout::{
    feature_names, split_feature_names, Feature, RolloutChannel, RolloutSnapshot,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use url::Url;

/// Compression mode policy for the `/v1/messages` endpoint.
///
/// Drives whether `compress_anthropic_request` does any work. PR-A1
/// (Phase A lockdown) wires the flag in but both modes currently
/// passthrough — `live_zone` parses-but-warns until Phase B PR-B2
/// fills in the live-zone-only block dispatcher.
///
/// We do NOT add an `icm` mode (the deleted code path) or a
/// `passthrough` alias for `off` — those names are misleading. The
/// only legal values are `off` (compression disabled) and `live_zone`
/// (compress only the live-zone blocks; not yet implemented).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum CompressionMode {
    /// Compression disabled. Body forwards byte-equal to upstream.
    /// This is the default; Phase B will switch the default to
    /// `live_zone` once that mode is implemented.
    Off,
    /// Compress only live-zone blocks (latest user message,
    /// latest tool/function/shell/patch outputs). NOT YET IMPLEMENTED:
    /// in PR-A1 this falls through to passthrough behaviour with a
    /// loud warning. Phase B PR-B2 wires in the actual dispatcher.
    LiveZone,
    /// Compress compressible blocks across ALL user messages, not just
    /// the latest. Deterministic per-content compression keeps identical
    /// content → identical bytes on every turn, so Anthropic's prompt
    /// cache forms over the compressed history (stable, no cascade).
    /// This is the subscription-savings mode (cuts cache_creation and
    /// the per-turn cache_read of compressed history).
    AllMessages,
}

/// Policy for stripping internal `x-headroom-*` headers from upstream-bound
/// requests (PR-A5, fixes P5-49).
///
/// When `enabled` (default), every header whose name starts with
/// `x-headroom-` is dropped before the upstream call. Stops fingerprinting
/// of the proxy via subscription-revocation flags (`x-headroom-bypass`,
/// `x-headroom-mode`, etc.) and prevents leakage of internal user-id /
/// stack / base-url headers.
///
/// When `disabled`, internal headers are forwarded verbatim. This is an
/// explicit operator opt-in for diagnostic shadow tracing — NOT a fallback.
/// Document the trade-off in `docs/configuration.md` before flipping this.
///
/// Source priority: CLI flag → `HEADROOM_PROXY_STRIP_INTERNAL_HEADERS`
/// env var → default (`enabled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum StripInternalHeaders {
    /// Strip every `x-headroom-*` header from upstream-bound requests.
    /// Default. Operationally safe.
    Enabled,
    /// Forward `x-headroom-*` to upstream verbatim. Diagnostic-only;
    /// exposes internal flags to the upstream and reveals the proxy.
    Disabled,
}

impl StripInternalHeaders {
    /// Stable snake_case name suitable for log fields.
    pub fn as_str(self) -> &'static str {
        match self {
            StripInternalHeaders::Enabled => "enabled",
            StripInternalHeaders::Disabled => "disabled",
        }
    }

    /// Convenience: is the strip switched on?
    pub fn is_enabled(self) -> bool {
        matches!(self, StripInternalHeaders::Enabled)
    }
}

/// Policy for automatically deriving `frozen_message_count` from the
/// customer's `cache_control` markers (PR-A4).
///
/// When `enabled` (default), the live-zone dispatcher will walk
/// `messages[*].content[*].cache_control` and bump the floor below
/// which compression is forbidden. When `disabled`, the floor stays
/// at 0 regardless of markers — Phase B's dispatcher will then treat
/// every message as live-zone, which is dangerous in production but
/// useful for benchmarking the cache-control machinery.
///
/// `system` and `tools[*]` markers never bump `frozen_count` because
/// those fields are *always* part of the cache hot zone (invariant I2);
/// they're guaranteed-immutable independently of marker placement.
///
/// Source priority: CLI flag → `HEADROOM_PROXY_CACHE_CONTROL_AUTO_FROZEN`
/// env var → default (`enabled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum CacheControlAutoFrozen {
    /// Walk customer `cache_control` markers and derive
    /// `frozen_message_count` automatically. Default.
    Enabled,
    /// Ignore customer `cache_control` markers when deriving
    /// `frozen_message_count`; the function returns 0 regardless of
    /// what the body contains. Intended for benchmarking and the
    /// "no automatic floor" testing path; not for production use.
    Disabled,
}

impl CacheControlAutoFrozen {
    /// Stable snake_case name suitable for log fields. Mirrors
    /// `CompressionMode::as_str` so the two policy fields render
    /// identically in JSON tracing output.
    pub fn as_str(self) -> &'static str {
        match self {
            CacheControlAutoFrozen::Enabled => "enabled",
            CacheControlAutoFrozen::Disabled => "disabled",
        }
    }

    /// Convenience: is the auto-frozen derivation switched on? Most
    /// callers want the boolean rather than pattern-matching on the
    /// enum.
    pub fn is_enabled(self) -> bool {
        matches!(self, CacheControlAutoFrozen::Enabled)
    }
}

/// Phase F PR-F2.1 c3/6: feature flag for the per-auth-mode
/// `CompressionPolicy` enforcement.
///
/// `disabled` (default until c6/6): the proxy still classifies
/// `auth_mode` and derives a `CompressionPolicy` for telemetry, but
/// every dispatcher and transform behaves as if the mode were `Payg`
/// — bit-for-bit current behaviour.
///
/// `enabled`: the policy struct's per-mode values take effect. For
/// Subscription specifically, the cache aligner is skipped and the
/// dispatcher gates on `policy.live_zone_compression_enabled()` (a
/// no-op in F2.1 since that helper currently always returns `true`,
/// but kept as a hook so F2.2 can flip without touching call sites).
///
/// Why a flag at all: F2.1 lands behind a default-disabled gate so
/// commits 4 and 5 of the PR don't ship behaviour change to default
/// users. Operators can flip this on for dogfooding before commit 6
/// flips the default. Rollback: flip the env var back to `disabled`
/// — instant if config is hot-reloaded, redeploy otherwise.
///
/// Source priority: CLI flag →
/// `HEADROOM_PROXY_AUTH_MODE_POLICY_ENFORCEMENT` env var →
/// default (`disabled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum AuthModePolicyEnforcement {
    /// Per-mode policy IS enforced. Subscription users see no
    /// cache_aligner; the dispatcher reads
    /// `policy.live_zone_compression_enabled()`.
    Enabled,
    /// Per-mode policy IS NOT enforced. Every mode runs the PAYG
    /// pipeline, identical to pre-F2.1 behaviour. Default in F2.1
    /// commits 1–5 so the feature is dogfood-only until c6/6.
    Disabled,
}

impl AuthModePolicyEnforcement {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthModePolicyEnforcement::Enabled => "enabled",
            AuthModePolicyEnforcement::Disabled => "disabled",
        }
    }

    pub fn is_enabled(self) -> bool {
        matches!(self, AuthModePolicyEnforcement::Enabled)
    }
}

impl CompressionMode {
    /// Stable snake_case name suitable for log fields. Avoids relying
    /// on `Debug` (which renders `Off`/`LiveZone`) or `Display`
    /// (which we don't implement to keep `ValueEnum` the single
    /// source of truth for stringification).
    pub fn as_str(self) -> &'static str {
        match self {
            CompressionMode::Off => "off",
            CompressionMode::LiveZone => "live_zone",
            CompressionMode::AllMessages => "all_messages",
        }
    }
}

/// Session-sticky provider beta headers (parity port of the Python
/// proxy's `HEADROOM_BETA_HEADER_STICKY`; see
/// `cache_stabilization::beta_sticky`).
///
/// When `enabled` (default), the proxy unions each request's
/// `anthropic-beta` / `openai-beta` tokens with the tokens previously
/// seen for the same conversation and forwards the union, so a client
/// dropping a beta token mid-conversation doesn't rotate the upstream
/// prefix-cache key.
///
/// When `disabled`, the client header is forwarded verbatim and no
/// per-session token state is kept. Diagnostic operator opt-in — NOT
/// a fallback per realignment build constraint #4.
///
/// Source priority: CLI flag → `HEADROOM_PROXY_BETA_HEADER_STICKY`
/// env var → default (`enabled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum BetaHeaderSticky {
    /// Union beta tokens per conversation and forward the union.
    /// Default. Matches the Python proxy's default behaviour.
    Enabled,
    /// Forward the client's beta header verbatim; keep no state.
    /// Diagnostic-only.
    Disabled,
}

impl BetaHeaderSticky {
    /// Stable snake_case name suitable for log fields.
    pub fn as_str(self) -> &'static str {
        match self {
            BetaHeaderSticky::Enabled => "enabled",
            BetaHeaderSticky::Disabled => "disabled",
        }
    }

    /// Convenience: is the sticky union switched on?
    pub fn is_enabled(self) -> bool {
        matches!(self, BetaHeaderSticky::Enabled)
    }
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "headroom-proxy",
    version,
    about = "Headroom transparent reverse proxy"
)]
pub struct CliArgs {
    /// Runtime rollout channel that bounds which managed features may run.
    ///
    /// `stable` admits only features that have completed bake time. `beta` and
    /// `canary` admit progressively newer features. `dev` is for local work.
    /// Explicit feature requests still cannot cross this boundary unless the
    /// unsafe override is set.
    #[arg(
        long = "rollout-channel",
        env = "HEADROOM_ROLLOUT_CHANNEL",
        default_value = "stable",
        value_parser = parse_rollout_channel,
    )]
    pub rollout_channel: String,

    /// Comma-separated rollout features to request explicitly.
    #[arg(
        long = "features",
        env = "HEADROOM_FEATURES",
        default_value = "",
        value_parser = parse_rollout_features,
    )]
    pub features: String,

    /// Comma-separated rollout features to force off. Disable wins over defaults
    /// and explicit enable requests.
    #[arg(
        long = "disable-features",
        env = "HEADROOM_DISABLE_FEATURES",
        default_value = "",
        value_parser = parse_rollout_features,
    )]
    pub disable_features: String,

    /// Break-glass override that allows unstable features below their channel.
    /// Intended only for emergency mitigation and should be visible in logs.
    #[arg(
        long = "unsafe-allow-unstable-features",
        env = "HEADROOM_UNSAFE_ALLOW_UNSTABLE_FEATURES",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub unsafe_allow_unstable_features: bool,

    /// Address the proxy listens on (e.g. 0.0.0.0:8787).
    #[arg(long, env = "HEADROOM_PROXY_LISTEN", default_value = "0.0.0.0:8787")]
    pub listen: SocketAddr,

    /// Upstream base URL the proxy forwards to (e.g. http://127.0.0.1:8788).
    /// REQUIRED — there is no default; we want operators to be explicit.
    #[arg(long, env = "HEADROOM_PROXY_UPSTREAM")]
    pub upstream: Url,

    /// End-to-end timeout for a single upstream request (long, since LLM
    /// streams may run for many minutes).
    #[arg(long, default_value = "600s", value_parser = parse_duration)]
    pub upstream_timeout: Duration,

    /// TCP/TLS connect timeout for upstream.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub upstream_connect_timeout: Duration,

    /// Optional HTTP proxy for upstream provider calls only (e.g.
    /// http://127.0.0.1:3128). Scoped to the proxy's provider HTTP
    /// client — it does NOT set process-wide `HTTP_PROXY`/`HTTPS_PROXY`
    /// env vars, which would leak into tool executions inheriting the
    /// environment. HTTP/2 is disabled for provider clients when this is
    /// set so HTTPS provider APIs can tunnel through a CONNECT proxy.
    #[arg(long, env = "HEADROOM_HTTP_PROXY")]
    pub http_proxy: Option<String>,

    /// Max body size for buffered cases (does NOT bound streaming bodies).
    #[arg(long, default_value = "100MB", value_parser = parse_bytes)]
    pub max_body_bytes: u64,

    /// Log level / filter (RUST_LOG-style). Default: info.
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Rewrite the outgoing Host header to the upstream host (default).
    /// Pair with --no-rewrite-host to preserve the client-supplied Host.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub rewrite_host: bool,

    /// Convenience flag matching the spec; sets rewrite_host=false when present.
    #[arg(long = "no-rewrite-host", default_value_t = false)]
    pub no_rewrite_host: bool,

    /// Maximum time to wait for in-flight requests to finish on shutdown.
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    pub graceful_shutdown_timeout: Duration,

    /// Enable Headroom compression on LLM-shaped requests
    /// (currently: `POST /v1/messages` for Anthropic). When off,
    /// the proxy stays a pure streaming passthrough.
    ///
    /// Off by default so existing operators get unchanged behaviour
    /// and the integration-test harness doesn't need to opt out
    /// per-test. Operators wanting to demo the compressor pass
    /// `--compression` (or set `HEADROOM_PROXY_COMPRESSION=1`).
    #[arg(
        long = "compression",
        env = "HEADROOM_PROXY_COMPRESSION",
        default_value_t = false
    )]
    pub compression: bool,

    /// Maximum body size to buffer for compression. Bodies larger
    /// than this get forwarded unchanged. Defaults to `--max-body-bytes`
    /// when unset, so operators only need to tune one knob unless
    /// they have a specific reason to cap compression separately.
    #[arg(long, value_parser = parse_bytes)]
    pub compression_max_body_bytes: Option<u64>,

    /// Compression mode policy for `/v1/messages`.
    ///
    /// `off`: byte-faithful passthrough on every request.
    /// `live_zone`: compress blocks in the latest user message only.
    /// `all_messages`: compress eligible blocks in every user message,
    /// deterministically, so identical content yields identical bytes
    /// wherever it sits in the history.
    ///
    /// Unset is not the same as `off`. When left unset the mode is
    /// resolved in [`Config::from_cli`]: `all_messages` if the
    /// interception path is on at all, `off` otherwise. Passing
    /// `--compression-mode off` explicitly always wins.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_COMPRESSION_MODE`
    /// env var → resolved default.
    #[arg(
        long = "compression-mode",
        env = "HEADROOM_PROXY_COMPRESSION_MODE",
        value_enum
    )]
    pub compression_mode: Option<CompressionMode>,

    /// Cross-turn (whole-conversation) verbatim de-dup for tool
    /// outputs. When a span in a later tool output already appeared
    /// verbatim in an earlier tool output, the later copy is replaced
    /// with a compact in-context pointer to the original
    /// (`[↑N L same as msg T: 'anchor']`, plus a `±D L` offset when the
    /// re-read was renumbered by an edit).
    /// Runs as a post-pass after the live-zone dispatcher, over the
    /// final block forms. Prefix-monotonic: appending a turn never
    /// rewrites an earlier turn's bytes, so the prompt-cache prefix
    /// stays stable. Frozen-prefix and `cache_control` blocks are
    /// reference targets only (never rewritten).
    ///
    /// Off by default — parity with the Python router's
    /// `enable_cross_turn_dedup: bool = False`.
    ///
    /// Source priority: CLI flag →
    /// `HEADROOM_PROXY_ENABLE_CROSS_TURN_DEDUP` env var → default
    /// (`false`).
    #[arg(
        long = "enable-cross-turn-dedup",
        env = "HEADROOM_PROXY_ENABLE_CROSS_TURN_DEDUP",
        default_value_t = false
    )]
    pub enable_cross_turn_dedup: bool,

    /// Inject Anthropic-native context-editing (`context_management`) into
    /// `/v1/messages` so subscription users get the server-side context GC
    /// (`clear_tool_uses`) that Claude Code gates behind ant-only flags.
    /// Adds the `context-management-2025-06-27` beta header. Off by default.
    #[arg(long = "context-edit", env = "HEADROOM_PROXY_CONTEXT_EDIT")]
    pub context_edit: bool,

    /// `clear_tool_uses`: keep this many most-recent tool results uncleared.
    #[arg(long = "context-edit-keep-tool-uses", default_value_t = 6)]
    pub context_edit_keep_tool_uses: u64,

    /// `clear_tool_uses`: fire once input tokens exceed this trigger.
    #[arg(long = "context-edit-trigger-tokens", default_value_t = 60_000)]
    pub context_edit_trigger_tokens: u64,

    /// `clear_tool_uses`: leave conversations shorter than this many messages
    /// alone.
    ///
    /// The first clear invalidates from the oldest tool result, so it re-creates
    /// nearly the whole history however `keep` is set — measured at 109,035
    /// creation tokens against ~1,836 weighted saved per turn, about 60 turns to
    /// pay back. A conversation that ends before then paid that fee for nothing,
    /// and the median conversation is 15 turns. This gate is what keeps the
    /// short ones out of it; it does not improve the payback ratio, which is
    /// structural.
    #[arg(long = "context-edit-min-messages", default_value_t = 40)]
    pub context_edit_min_messages: usize,

    /// `clear_tool_uses`: skip the strategy entirely unless it can clear at
    /// least this many tokens. Stops a small clear from buying a full cache
    /// write; below the floor the cached prefix survives untouched.
    #[arg(long = "context-edit-clear-at-least")]
    pub context_edit_clear_at_least: Option<u64>,

    /// `clear_thinking`: keep thinking from this many most-recent assistant
    /// turns and let the server drop the rest. Must be > 0.
    ///
    /// Claude Code sends this edit itself on every request with `keep: "all"`,
    /// so setting this OVERRIDES the client's value — the whole point, since
    /// `"all"` clears nothing. Only worth it on models that bill prior turns'
    /// thinking as input (Opus 4.5, and 4.6 and later); Sonnet 4.5 and earlier
    /// stripped it for free.
    ///
    /// Measured on 8 deep bodies: `1` removes 12.2% of input tokens. Larger
    /// values save less AND invalidate more, because the newly-cleared block
    /// sits further from the tail. See `docs/context-editing-api-facts.md`.
    #[arg(long = "context-edit-keep-thinking")]
    pub context_edit_keep_thinking: Option<u64>,

    /// Tool pruning (A4): drop whole MCP servers by name (comma-separated).
    /// A tool named `mcp__<server>__<fn>` is dropped when `<server>` is listed.
    /// Deterministic + cache-safe; never touches built-in (non-MCP) tools.
    /// Off by default. Reduces cache_creation — the only bucket that counts
    /// toward subscription usage (cache reads are free per Anthropic docs).
    #[arg(long = "prune-drop-mcp", env = "HEADROOM_PROXY_PRUNE_DROP_MCP")]
    pub prune_drop_mcp: Option<String>,

    /// Tool pruning (A4): drop these exact tool names (comma-separated),
    /// built-in ones included — the gap `--prune-drop-mcp` leaves. Matching is
    /// exact, never by prefix, so the survivors are predictable.
    /// Deterministic + cache-safe; composes with `--prune-drop-mcp`, and
    /// `--prune-keep-tools` still wins over both. Off by default.
    /// Aimed at built-ins Claude Code ships but the session never calls: over
    /// 374 captured requests `ListMcpResourcesTool`, `ReadMcpResourceTool` and
    /// `ReadMcpResourceDirTool` were invoked 0 times out of 21,863 tool calls,
    /// yet whether they were present split 64% of traffic into two tools
    /// fingerprints — and each fingerprint pays its own cache_creation.
    #[arg(long = "prune-drop-tools", env = "HEADROOM_PROXY_PRUNE_DROP_TOOLS")]
    pub prune_drop_tools: Option<String>,

    /// Tool pruning (A4): keep ONLY these tool names (comma-separated
    /// allowlist). When set, every tool not listed is dropped (most
    /// aggressive); takes precedence over --prune-drop-mcp. Off by default.
    #[arg(long = "prune-keep-tools", env = "HEADROOM_PROXY_PRUNE_KEEP_TOOLS")]
    pub prune_keep_tools: Option<String>,

    /// Whether to derive `frozen_message_count` from customer
    /// `cache_control` markers in the request body (PR-A4).
    ///
    /// `enabled` (default): walk `messages[*].content[*].cache_control`
    /// and bump the floor for live-zone compression so any message
    /// the customer cache-pinned is left untouched. `disabled`: skip
    /// the walk; the floor stays at 0. The off switch exists for
    /// benchmark setups that want to measure compression independent
    /// of marker placement; it is NOT recommended for production.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_CACHE_CONTROL_AUTO_FROZEN`
    /// env var → default (`enabled`).
    #[arg(
        long = "cache-control-auto-frozen",
        env = "HEADROOM_PROXY_CACHE_CONTROL_AUTO_FROZEN",
        value_enum,
        default_value_t = CacheControlAutoFrozen::Enabled,
    )]
    pub cache_control_auto_frozen: CacheControlAutoFrozen,

    /// Phase F PR-F2.1 c5/5: per-auth-mode `CompressionPolicy`
    /// enforcement is now ON by default. Subscription users skip
    /// CacheAligner; PAYG/OAuth keep current behaviour. Operators
    /// can flip back to `disabled` via the env var if F2.1 surfaces
    /// any subscription regression.
    ///
    /// Source priority: CLI flag →
    /// `HEADROOM_PROXY_AUTH_MODE_POLICY_ENFORCEMENT` env var →
    /// default (`enabled` from c5/5 onward).
    #[arg(
        long = "auth-mode-policy-enforcement",
        env = "HEADROOM_PROXY_AUTH_MODE_POLICY_ENFORCEMENT",
        value_enum,
        default_value_t = AuthModePolicyEnforcement::Enabled,
    )]
    pub auth_mode_policy_enforcement: AuthModePolicyEnforcement,

    /// Strip internal `x-headroom-*` headers from upstream-bound
    /// requests (PR-A5, fixes P5-49). Default `enabled`. The `disabled`
    /// path is operator opt-in for diagnostic shadow tracing only —
    /// NOT a fallback per realignment build constraint #4.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_STRIP_INTERNAL_HEADERS`
    /// env var → default (`enabled`).
    #[arg(
        long = "strip-internal-headers",
        env = "HEADROOM_PROXY_STRIP_INTERNAL_HEADERS",
        value_enum,
        default_value_t = StripInternalHeaders::Enabled,
    )]
    pub strip_internal_headers: StripInternalHeaders,

    /// Session-sticky provider beta headers: union `anthropic-beta` /
    /// `openai-beta` tokens per conversation so a client dropping a
    /// token mid-conversation doesn't bust the upstream prefix cache.
    /// Parity port of the Python proxy's `SessionBetaTracker` (PR-A6).
    /// Default `enabled`; `disabled` is a diagnostic operator opt-in.
    ///
    /// Active only when the compression interceptor is on
    /// (`--compression` / `HEADROOM_PROXY_COMPRESSION=1`): with the
    /// interceptor off the proxy is a strict byte-pipe and never
    /// mutates headers. Startup logs a warning when this is `enabled`
    /// while `--compression` is off.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_BETA_HEADER_STICKY`
    /// env var → default (`enabled`).
    #[arg(
        long = "beta-header-sticky",
        env = "HEADROOM_PROXY_BETA_HEADER_STICKY",
        value_enum,
        default_value_t = BetaHeaderSticky::Enabled,
    )]
    pub beta_header_sticky: BetaHeaderSticky,

    /// Phase C PR-C4: enable the `/v1/responses` SSE streaming
    /// pipeline. When `true` (default), `Accept: text/event-stream`
    /// requests on `/v1/responses` flow through the byte-level SSE
    /// framer + Responses state-machine telemetry tee that PR-C1
    /// wired into `forward_http`'s response stream. When `false`,
    /// the streaming pipeline is bypassed and the SSE response is
    /// proxied as opaque bytes (no framer, no state machine,
    /// strictly fewer logs). Bypass exists ONLY for emergency
    /// rollback of the streaming pipeline without flipping the
    /// global `--compression` switch — it is NOT a fallback path.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_ENABLE_RESPONSES_STREAMING`
    /// env var → default (`true`).
    #[arg(
        long = "enable-responses-streaming",
        env = "HEADROOM_PROXY_ENABLE_RESPONSES_STREAMING",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub enable_responses_streaming: bool,

    /// Phase C PR-C4: enable the `/v1/conversations*` passthrough
    /// surface. When `true` (default), the proxy mounts explicit
    /// axum routes for OpenAI's Conversations API
    /// (`POST/GET/DELETE /v1/conversations/...` and the nested
    /// `/items` paths) and forwards every request upstream
    /// byte-equal with structured-log instrumentation
    /// (`event = "conversations_passthrough_pr_c4"`). When `false`,
    /// requests still reach upstream via the catch-all but lose
    /// the per-route logging. Compression on conversation items
    /// is NOT performed in this PR — `enable_conversations_passthrough`
    /// is strictly an instrumentation switch.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_ENABLE_CONVERSATIONS_PASSTHROUGH`
    /// env var → default (`true`).
    #[arg(
        long = "enable-conversations-passthrough",
        env = "HEADROOM_PROXY_ENABLE_CONVERSATIONS_PASSTHROUGH",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub enable_conversations_passthrough: bool,

    /// Enable batch API routes (Google batchGenerateContent, OpenAI batch).
    /// When `false` (default), batch requests fall through to the catch-all
    /// and forward byte-equal to upstream without compression.
    #[arg(
        long = "enable-batch-api",
        env = "HEADROOM_PROXY_ENABLE_BATCH_API",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub enable_batch_api: bool,

    /// Phase D PR-D1: enable the native Bedrock InvokeModel route.
    /// When `true` (default), `POST /model/{model_id}/invoke` is
    /// handled by the Rust `bedrock::invoke` handler — Anthropic-shape
    /// bodies run through the live-zone compression path and the
    /// proxy re-signs the request with SigV4 before forwarding to
    /// the configured Bedrock endpoint. When `false`, the routes are
    /// not mounted and requests fall through to the catch-all
    /// (which forwards to `--upstream` byte-equal but does NOT
    /// re-sign — operators MUST run an unsigned upstream that
    /// happens to know what to do, otherwise this fails closed).
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_ENABLE_BEDROCK_NATIVE`
    /// env var → default (`true`).
    #[arg(
        long = "enable-bedrock-native",
        env = "HEADROOM_PROXY_ENABLE_BEDROCK_NATIVE",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub enable_bedrock_native: bool,

    /// Enable the Kompress ML prose compressor for `PlainText` blocks in the
    /// live zone. Default `false`: Kompress loads a ~261 MB ONNX model
    /// (resolved CACHE-ONLY — the proxy never downloads it), so — unlike the
    /// always-on structural compressors and the AST CodeCompressor — operators
    /// opt in. When `false`, plain-text blocks pass through untouched and the
    /// model is never loaded. Mirrors the Python reference's `enable_kompress`.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_ENABLE_KOMPRESS`
    /// env var → default (`false`).
    #[arg(
        long = "enable-kompress",
        env = "HEADROOM_PROXY_ENABLE_KOMPRESS",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub enable_kompress: bool,

    /// CTX-2: enable passive session capture (conversation identity +
    /// event extraction into the sessions DB). Pure observer — never mutates
    /// or delays a request. Off by default; operators opt in.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_CTX_CAPTURE`
    /// env var → default (`false`).
    #[arg(
        long = "ctx-capture",
        env = "HEADROOM_PROXY_CTX_CAPTURE",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub ctx_capture: bool,

    /// CTX-2: base directory for the sessions/content DBs. When unset, defaults
    /// to `<workspace>/ctx` (`~/.headroom/ctx`). Only consulted when
    /// `ctx_capture` is enabled. Point this at
    /// `~/.claude-personal/context-mode` to keep reading a store written before
    /// the default moved under the workspace root.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_CTX_STORE_DIR` env var →
    /// default.
    #[arg(long = "ctx-store-dir", env = "HEADROOM_PROXY_CTX_STORE_DIR")]
    pub ctx_store_dir: Option<std::path::PathBuf>,

    /// CTX-3: enable the tool_result offload transform. Replaces oversized
    /// `tool_result` blocks with a deterministic structural digest before the
    /// live-zone compressors run, and stashes the original in the CCR store for
    /// retrieval. Pure function of block bytes (cache-safe). Default `false`.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_CTX_OFFLOAD` env → default.
    #[arg(
        long = "ctx-offload",
        env = "HEADROOM_PROXY_CTX_OFFLOAD",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub ctx_offload: bool,

    /// Ceiling on bytes one request may gain across every injection stage —
    /// CCR proactive expansion, ctx recall, and memory — together.
    ///
    /// Each stage has its own cap (expansion count, result count, entry
    /// count), and before this flag nothing summed them: three individually
    /// small appenders could still inflate one turn while every per-stage
    /// counter reported success. `0` turns all three off.
    ///
    /// Recall is reserved against but never clipped — it is replayed
    /// byte-for-byte into the cached prefix, so cutting it would bust the
    /// cache. See `injection_budget.rs`.
    #[arg(
        long = "max-injection-bytes",
        env = "HEADROOM_MAX_INJECTION_BYTES",
        default_value_t = crate::injection_budget::DEFAULT_MAX_INJECTION_BYTES,
    )]
    pub max_injection_bytes: usize,

    /// Memory system master switch. Default `false`.
    ///
    /// Source priority: CLI flag → `HEADROOM_MEMORY_ENABLED` env → default.
    /// Before this flag existed the switch was env-only, so a launcher that
    /// passed no environment left the whole subsystem off with nothing on the
    /// request path to say so.
    #[arg(
        long = "memory",
        env = "HEADROOM_MEMORY_ENABLED",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub memory_enabled: bool,

    /// Freeze-replay: replay the previously-forwarded (compressed) prefix
    /// byte-identical each turn so the provider prompt cache stays warm
    /// (ports Python `PrefixCacheTracker` / `overlay_cached_prefix`). Anthropic
    /// only. Default `false` — staged rollout; when off the request path is
    /// byte-for-byte unchanged.
    #[arg(
        long = "prefix-replay",
        env = "HEADROOM_PROXY_PREFIX_REPLAY",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub prefix_replay: bool,

    /// How many `cache_control` breakpoints to place on the tail of `messages`,
    /// counting back from the newest. Only read on the freeze-replay path, which
    /// owns message-level placement.
    ///
    /// `1` is the placement this proxy has always used and the default. `2` adds
    /// a hedge: when the newest message changes the older marker still names a
    /// prefix the provider holds, so the read starts there instead of at
    /// nothing. Published multipliers put two slots ~5% below one.
    ///
    /// Anthropic refuses more than 4 markers across `system`, `tools` and
    /// `messages` together. Claude Code sends 2 on `system`, so `2` here reaches
    /// that ceiling exactly — pair it with `--strip-system-cache-breakpoints`.
    #[arg(
        long = "cache-tail-breakpoints",
        env = "HEADROOM_PROXY_CACHE_TAIL_BREAKPOINTS",
        default_value_t = 1,
        value_parser = clap::value_parser!(u8).range(1..=4),
    )]
    pub cache_tail_breakpoints: u8,

    /// Drop the `cache_control` markers the client set on `system`. Only read on
    /// the freeze-replay path, and only honoured once a message breakpoint is
    /// actually placed — with none placed these are the request's only markers
    /// and removing them turns caching off.
    ///
    /// A breakpoint caches everything before it and the system prompt precedes
    /// every message, so the tail marker already covers it. Claude Code's two
    /// ask for the 1h TTL, which writes at 2.0x against 5m's 1.25x. Default
    /// `false`: this reaches past `messages[*]` into what the client set, which
    /// nothing else here does.
    #[arg(
        long = "strip-system-cache-breakpoints",
        env = "HEADROOM_PROXY_STRIP_SYSTEM_CACHE_BREAKPOINTS",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub strip_system_cache_breakpoints: bool,

    /// B2: replay the tool order forwarded last turn and append genuinely-new
    /// tools at the end, so a late MCP handshake splicing definitions into the
    /// middle of `tools[]` does not invalidate the cached prefix behind them.
    /// Anthropic only. Lossless — the same definitions go out, byte for byte,
    /// in a different order. Default `true`; self-disables whenever a tool
    /// carries a `cache_control` marker, which on PAYG hands ordering back to
    /// PR-E1's alphabetic sort.
    ///
    /// Only runs when the proxy is already buffering the body (`compression`
    /// or any of the ctx flags). A pure-passthrough proxy never parses the
    /// request, so there is nothing to stabilize and the client's bytes go out
    /// untouched.
    #[arg(
        long = "cache-stable-tool-order",
        env = "HEADROOM_PROXY_CACHE_STABLE_TOOL_ORDER",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub cache_stable_tool_order: bool,

    /// B1: rewrite every `cache_control` marker to `ttl: "1h"` so the cached
    /// prefix survives idle gaps past the 5-minute default. Anthropic only,
    /// and skipped on PAYG — a 1h write is priced at 2× base input against
    /// 1.25× for 5m, so it is free on a subscription (where writes are
    /// token-counted for the usage window) and 60% dearer in dollars on an
    /// API key.
    ///
    /// Default `false`. The 5-minute cache refreshes on every use at no cost,
    /// so this only ever rescues a gap since the last touch; whether that is
    /// worth a one-time cache creation per conversation depends on an idle
    /// pattern only the operator can see.
    #[arg(
        long = "force-1h-cache-ttl",
        env = "HEADROOM_PROXY_FORCE_1H_CACHE_TTL",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub force_1h_cache_ttl: bool,

    /// Move Claude Code's single message breakpoint onto the last content
    /// block when it sits short of it.
    ///
    /// Default `true`. A no-op on the 97% of captured requests where the
    /// client already placed it there, and worth -0.9% of the bill under API
    /// weights and -3.5% under subscription weights on the rest. See
    /// [`crate::cache_stabilization::message_breakpoints`].
    #[arg(
        long = "cache-tail-breakpoint",
        env = "HEADROOM_PROXY_CACHE_TAIL_BREAKPOINT",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub cache_tail_breakpoint: bool,

    /// Split the cache TTL: 1h on the tools and system prefix, 5m on messages.
    ///
    /// Takes precedence over `--force-1h-cache-ttl`, which pins everything to
    /// the 2.0x tier including the moving message tail — content the next turn
    /// supersedes within seconds, which never needed an hour. See
    /// [`crate::cache_stabilization::cache_ttl::tail_5m_prefix_1h`].
    ///
    /// Default `false`, and measured at +511% depth-standardised creation on
    /// live traffic on 2026-08-17 — see
    /// [`crate::cache_stabilization::cache_ttl::tail_5m_prefix_1h`] for the
    /// numbers and the mechanism. Do not enable without a live A/B.
    #[arg(
        long = "split-cache-ttl",
        env = "HEADROOM_PROXY_SPLIT_CACHE_TTL",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub split_cache_ttl: bool,

    /// Persist forwarded prefixes here so a proxy restart does not throw them
    /// away. Empty (the default) keeps them in memory only.
    ///
    /// A restart empties the replay store, and the miss that follows is not
    /// free: with no tracker, compression stops treating the history as frozen
    /// and rewrites it, so the bytes stop matching the prefix the provider still
    /// holds. Measured at 352,167 tokens over 7 turns — 10% of all failed
    /// re-use — every one within minutes of a proxy start. See
    /// [`crate::cache_stabilization::prefix_replay::SessionReplayStore::with_persistence`].
    #[arg(
        long = "replay-store-dir",
        env = "HEADROOM_PROXY_REPLAY_STORE_DIR",
        default_value = ""
    )]
    pub replay_store_dir: String,

    /// Hold the working-directory line in the `system` preamble to the value
    /// each conversation opened with, and state the live one at the message
    /// tail.
    ///
    /// The line sits inside every cached prefix and carries no marker of its
    /// own, so a `cd` re-creates the conversation from the system block down: one
    /// such edit cost 65,051 tokens against a depth-peer average of 4,637 in the
    /// 2026-08-17 capture. See
    /// [`crate::cache_stabilization::working_dir`], which also explains why the
    /// live directory is restated rather than dropped.
    ///
    /// Default `false`. It is the only mechanism here that adds text the client
    /// did not send, so it stays opt-in.
    #[arg(
        long = "hold-working-directory",
        env = "HEADROOM_PROXY_HOLD_WORKING_DIRECTORY",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub hold_working_directory: bool,

    /// CTX-3: minimum serialized byte length a `tool_result` block must exceed
    /// to be offloaded. Static per invariant I3 (never changes mid-session).
    /// Default `50_000` (mirrors context-mode's Read threshold).
    #[arg(
        long = "ctx-offload-min-bytes",
        env = "HEADROOM_PROXY_CTX_OFFLOAD_MIN_BYTES",
        default_value_t = 50_000
    )]
    pub ctx_offload_min_bytes: usize,

    /// CTX-3: how many messages back from the tail a `tool_result` must be
    /// before `--exclude-tools` stops shielding it from offload. `0` (the
    /// default) shields the whole history, which is the behaviour before this
    /// flag existed.
    ///
    /// `--exclude-tools` keeps file and search results verbatim so the model
    /// never edits a file from a summary of it. Offload is not a summary — the
    /// original is retrievable by hash — and the risk it guards against is
    /// about the results in play, not the ones twenty messages back. Those were
    /// 9.9% of a mean prompt as raw `Read` output alone on 2026-08-17, never
    /// once digested.
    ///
    /// Distance from the tail grows, so this predicate turns true under blocks
    /// already inside the cached prefix. Safe only because a first conversion
    /// waits for a rebuild boundary and a converted block never reverts; see
    /// `CtxOffloadConfig::stale_margin`. Do not reimplement the test without
    /// both.
    #[arg(
        long = "ctx-offload-stale-messages",
        env = "HEADROOM_PROXY_CTX_OFFLOAD_STALE_MESSAGES",
        default_value_t = 0
    )]
    pub ctx_offload_stale_messages: usize,

    /// CTX-3: how many messages past `--ctx-offload-stale-messages` a first
    /// conversion may happen on an ordinary turn rather than waiting for a
    /// rebuild boundary. `0` (the default) always waits.
    ///
    /// This is the one deliberate cache cost in the offload path. Converting a
    /// block inside the cached prefix rewrites everything after it, and at 1.45
    /// for creation against 0.09 for reads the trade needs
    /// `16.1 * tokens_after / tokens_saved` turns to pay off. Deep in the history
    /// that is hundreds of turns; just past the margin it is ten, because the
    /// last 4 messages are only 1,460 tokens (median, 3,545 bodies at depth ≥ 20,
    /// 2026-08-17) while a qualifying block there saves 2,280.
    ///
    /// Keep it narrow. The last 8 messages are already 3,699 tokens, so widening
    /// this raises the cost faster than the saving.
    #[arg(
        long = "ctx-offload-stale-window",
        env = "HEADROOM_PROXY_CTX_OFFLOAD_STALE_WINDOW",
        default_value_t = 0
    )]
    pub ctx_offload_stale_window: usize,

    /// CTX-3: TTL (seconds) for offloaded originals in the CCR store. Long by
    /// design (retrieval outlives a session); default `604_800` (7 days).
    #[arg(
        long = "ctx-offload-ttl-seconds",
        env = "HEADROOM_PROXY_CTX_OFFLOAD_TTL_SECONDS",
        default_value_t = 604_800
    )]
    pub ctx_offload_ttl_seconds: u64,

    /// CTX-4: enable recall/resume injection. On the first request of a
    /// conversation, prepends a recall block (fresh) or resume snapshot
    /// (compaction/resume) into the first user message, then replays the same
    /// bytes verbatim on every later turn (invariant I4). Requires
    /// `ctx_capture` (the identity/sessions layer) — enforced loudly at
    /// startup. Default `false`.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_CTX_INJECT` env → default.
    #[arg(
        long = "ctx-inject",
        env = "HEADROOM_PROXY_CTX_INJECT",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    pub ctx_inject: bool,

    /// CCR Phase 4: track compressed/offloaded content across turns so later
    /// user queries can proactively retrieve relevant originals. Requires
    /// `--ctx-offload`; when offload is unavailable this is a no-op.
    #[arg(
        long = "ccr-context-tracking",
        env = "HEADROOM_PROXY_CCR_CONTEXT_TRACKING",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub ccr_context_tracking: bool,

    /// CCR Phase 4: proactively append relevant previously-offloaded content
    /// to the latest user turn before forwarding the request upstream.
    #[arg(
        long = "ccr-proactive-expansion",
        env = "HEADROOM_PROXY_CCR_PROACTIVE_EXPANSION",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub ccr_proactive_expansion: bool,

    /// CCR Phase 4: maximum proactive expansions appended to one request.
    #[arg(
        long = "ccr-max-proactive-expansions",
        env = "HEADROOM_PROXY_CCR_MAX_PROACTIVE_EXPANSIONS",
        default_value_t = 2
    )]
    pub ccr_max_proactive_expansions: usize,

    /// AWS region to use when signing Bedrock requests. Default
    /// `us-east-1`. The Bedrock endpoint URL derived from this
    /// region is `https://bedrock-runtime.{region}.amazonaws.com`
    /// (override via `--bedrock-endpoint` for FIPS or VPC endpoints).
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_BEDROCK_REGION`
    /// env var → `AWS_REGION` env var → default (`us-east-1`).
    #[arg(
        long = "bedrock-region",
        env = "HEADROOM_PROXY_BEDROCK_REGION",
        default_value = "us-east-1"
    )]
    pub bedrock_region: String,

    /// Bedrock endpoint base URL. When unset (the common case), the
    /// proxy derives `https://bedrock-runtime.{bedrock_region}.amazonaws.com`
    /// from the configured region. Override for FIPS endpoints
    /// (`bedrock-runtime-fips.{region}.amazonaws.com`), VPC endpoints,
    /// or local-mock test setups.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_BEDROCK_ENDPOINT`
    /// env var → derived-from-region.
    #[arg(long = "bedrock-endpoint", env = "HEADROOM_PROXY_BEDROCK_ENDPOINT")]
    pub bedrock_endpoint: Option<Url>,

    /// AWS profile name passed to the `aws-config` default credential
    /// chain. When unset, the chain uses the default behaviour
    /// (env vars → `[default]` profile → IMDS / ECS task role).
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_AWS_PROFILE`
    /// env var → `AWS_PROFILE` env var → default chain.
    #[arg(long = "aws-profile", env = "HEADROOM_PROXY_AWS_PROFILE")]
    pub aws_profile: Option<String>,

    /// Phase D PR-D2: validate the prelude + message CRC32 on each
    /// inbound Bedrock EventStream frame. Default `true` — production
    /// MUST validate. Operators flip to `false` ONLY for debugging a
    /// suspected wire-format issue (e.g. a corrupt-but-cooperative
    /// upstream that emits invalid CRCs intentionally). When disabled,
    /// the proxy still parses message boundaries; it just doesn't
    /// reject on CRC mismatch. Per project policy, every flag flip
    /// is logged at app-build time.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_BEDROCK_VALIDATE_EVENTSTREAM_CRC`
    /// env var → default (`true`).
    #[arg(
        long = "bedrock-validate-eventstream-crc",
        env = "HEADROOM_PROXY_BEDROCK_VALIDATE_EVENTSTREAM_CRC",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub bedrock_validate_eventstream_crc: bool,

    /// Phase D PR-D4: GCP Vertex region for the publisher path
    /// (`{region}-aiplatform.googleapis.com`). Default `us-central1`
    /// (matches the GCP-published default region for Anthropic
    /// publisher models). The proxy does NOT auto-construct the
    /// regional URL — that's an `--upstream` decision the operator
    /// makes once at startup. This flag is exposed for structured
    /// logging + observability so dashboards can group Vertex traffic
    /// by region without parsing the upstream URL.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_VERTEX_REGION`
    /// env var → default (`us-central1`).
    #[arg(
        long = "vertex-region",
        env = "HEADROOM_PROXY_VERTEX_REGION",
        default_value = "us-central1"
    )]
    pub vertex_region: String,

    /// Phase D PR-D4: OAuth scope to request from GCP ADC. Defaults
    /// to `cloud-platform`, the broad scope `gcloud` itself uses for
    /// ADC. Operators with tighter IAM postures can scope down to
    /// `cloud-platform.read-only` etc., but Vertex `:rawPredict`
    /// requires write so most deployments use the default.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_VERTEX_ADC_SCOPE`
    /// env var → default (`cloud-platform`).
    #[arg(
        long = "vertex-adc-scope",
        env = "HEADROOM_PROXY_VERTEX_ADC_SCOPE",
        default_value = "https://www.googleapis.com/auth/cloud-platform"
    )]
    pub vertex_adc_scope: String,

    /// Route requests for a local model through a local upstream with
    /// Anthropic↔OpenAI format translation. When set, any `/v1/messages`
    /// request whose `model` field matches this value is translated to
    /// OpenAI Chat Completions format and forwarded to `--local-upstream`.
    /// All other requests pass through transparently.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_LOCAL_MODEL` env var →
    /// default (None = disabled).
    #[arg(long = "local-model", env = "HEADROOM_PROXY_LOCAL_MODEL")]
    pub local_model: Option<String>,

    /// Upstream URL for the local model (e.g. http://localhost:8080).
    /// Required when `--local-model` is set; the proxy appends
    /// `/v1/chat/completions` to this base. Ignored when `--local-model`
    /// is unset.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_LOCAL_UPSTREAM` env var →
    /// default (None).
    #[arg(long = "local-upstream", env = "HEADROOM_PROXY_LOCAL_UPSTREAM")]
    pub local_upstream: Option<Url>,

    /// Additional model routes. Each value is
    /// `MODEL_NAME=UPSTREAM_URL[:openai[:TARGET_MODEL_ID]]`, where `:openai`
    /// (or its older spelling `:translate`) means "translate Anthropic format
    /// to OpenAI format". Model names ending in `*` are prefix-matched. Can be
    /// given multiple times or as a comma-separated list in
    /// `HEADROOM_PROXY_EXTRA_MODEL_ROUTES`.
    ///
    /// `TARGET_MODEL_ID` does two things. It overrides the model id sent
    /// upstream, so `MODEL_NAME` can be a discoverable `claude-*` id (Claude
    /// Code's gateway discovery only lists ids starting with `claude` or
    /// `anthropic`) while the real upstream id differs. It ALSO selects the
    /// endpoint: with a target the request goes to the agentic Responses API
    /// (`/v1/responses`, or the ChatGPT Codex backend under Codex auth);
    /// without one it goes to `/v1/chat/completions`. Codex needs the former.
    ///
    /// Examples:
    ///   --extra-model-route "MiMo-V2.5=https://api.xiaomimimo.com/anthropic"
    ///   --extra-model-route "codex-*=https://api.openai.com/v1:openai"
    ///   --extra-model-route "claude-codex-terra=https://api.openai.com/v1:openai:gpt-5.6-terra"
    #[arg(long = "extra-model-route", env = "HEADROOM_PROXY_EXTRA_MODEL_ROUTES")]
    pub extra_model_routes: Vec<String>,

    /// Path to a Codex auth JSON file (default: ~/.codex/auth.json).
    /// When set and the file exists, the proxy reads the access_token
    /// and uses it as a Bearer token for OpenAI upstream requests.
    /// The token is re-read from disk on each request so Codex's
    /// refresh cycle is picked up without proxy restarts.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_CODEX_AUTH_FILE` env var →
    /// default (`~/.codex/auth.json` if it exists, None otherwise).
    #[arg(long = "codex-auth-file", env = "HEADROOM_PROXY_CODEX_AUTH_FILE")]
    pub codex_auth_file: Option<String>,

    /// Proxy run mode: "token" (prioritize compression) or "cache"
    /// (prioritize provider prefix cache stability). Aliases like
    /// "token_headroom", "cost_savings" are normalized automatically.
    #[arg(long = "mode", env = "HEADROOM_MODE", default_value = "token")]
    pub mode: String,

    /// Master switch for output-token shaping. When enabled, the proxy
    /// appends verbosity steering to system prompts and routes effort
    /// on mechanical tool-result continuations.
    #[arg(
        long = "output-shaper",
        env = "HEADROOM_OUTPUT_SHAPER",
        default_value_t = false
    )]
    pub output_shaper_enabled: bool,

    /// Verbosity steering level 0-4. 0 = off, 1 = skip preamble,
    /// 2 = default, 3 = conclusions only, 4 = minimum tokens.
    #[arg(
        long = "verbosity-level",
        env = "HEADROOM_VERBOSITY_LEVEL",
        default_value_t = 2
    )]
    pub verbosity_level: i32,

    /// Effort value for mechanical tool-result continuations.
    /// Only effective when output-shaper is enabled.
    #[arg(
        long = "mechanical-effort",
        env = "HEADROOM_MECHANICAL_EFFORT",
        default_value = "low"
    )]
    pub mechanical_effort: String,

    /// Enable semantic response caching. When `true`, identical
    /// non-streaming requests are served from an in-memory LRU cache
    /// instead of hitting upstream. Cache keys hash `{model, messages,
    /// system, tools, ...}` with `cache_control` annotations stripped
    /// so moved breakpoints don't fragment keys.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_CACHE_ENABLED`
    /// env var → default (`true`).
    #[arg(
        long = "cache",
        env = "HEADROOM_PROXY_CACHE_ENABLED",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub cache_enabled: bool,

    /// TTL for cached responses in seconds. Entries older than this
    /// are evicted on access.
    #[arg(
        long = "cache-ttl",
        env = "HEADROOM_PROXY_CACHE_TTL",
        default_value_t = 3600
    )]
    pub cache_ttl_seconds: u64,

    /// Maximum number of entries in the semantic response cache.
    /// Older entries are evicted LRU when the limit is reached.
    #[arg(
        long = "cache-max-entries",
        env = "HEADROOM_PROXY_CACHE_MAX_ENTRIES",
        default_value_t = 1000
    )]
    pub cache_max_entries: usize,

    /// CCR: inject the `headroom_retrieve` tool definition into
    /// outgoing LLM requests. Default `true`.
    #[arg(
        long = "ccr-inject-tool",
        env = "HEADROOM_CCR_INJECT_TOOL",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub ccr_inject_tool: bool,

    /// CCR: auto-handle `headroom_retrieve` tool calls server-side
    /// (retrieve context and continue the conversation). Default `true`.
    #[arg(
        long = "ccr-handle-responses",
        env = "HEADROOM_CCR_HANDLE_RESPONSES",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub ccr_handle_responses: bool,

    /// CCR: max rounds of retrieve-continue per response. Default `8`.
    #[arg(
        long = "ccr-max-retrieval-rounds",
        env = "HEADROOM_CCR_MAX_RETRIEVAL_ROUNDS",
        default_value_t = 8
    )]
    pub ccr_max_retrieval_rounds: usize,

    /// Retry upstream requests on transient errors (429, 5xx).
    /// Default `true`.
    #[arg(
        long = "retry",
        env = "HEADROOM_RETRY_ENABLED",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub retry_enabled: bool,

    /// Max retry attempts per upstream call. Default `3`.
    #[arg(
        long = "retry-max-attempts",
        env = "HEADROOM_RETRY_MAX_ATTEMPTS",
        default_value_t = 3
    )]
    pub retry_max_attempts: u32,

    /// Attempts for a 200 response whose SSE body opens with an error event.
    /// Default `6`.
    ///
    /// Anthropic reports overload inside the body when the client asked for a
    /// stream, and those outages run far longer than a transport blip: across
    /// 77 turns the proxy gave up on, the bursts lasted 27 to 245 seconds. At
    /// three attempts the loop waits about 3 seconds and clears 21% of them.
    /// Six waits about 31 seconds and clears 69%, which is where the curve
    /// bends — a seventh attempt doubles the wait for five more points.
    ///
    /// Retrying here is free of duplication risk: the error is the *first*
    /// event, so nothing has been forwarded and the bytes are still ours.
    #[arg(
        long = "retry-overload-max-attempts",
        env = "HEADROOM_RETRY_OVERLOAD_MAX_ATTEMPTS",
        default_value_t = 6
    )]
    pub retry_overload_max_attempts: u32,

    /// Bytes of a streamed response held back before it counts as committed.
    /// Default `2048`. `0` disables the mid-stream retry.
    ///
    /// A body that dies mid-stream cannot be re-sent blind: across 18 observed
    /// drops the parser had a content block open every time, 1 to 20 output
    /// tokens in, so a second attempt would splice two generations together.
    /// Holding the opening bytes keeps the response uncommitted long enough to
    /// start over instead, and 2 KiB covers the preamble plus roughly a dozen
    /// deltas — past every drop seen so far.
    ///
    /// The trade is time to first paint: this much of every stream arrives in
    /// one burst rather than token by token. Raise it to cover later drops,
    /// lower it to hand the first token over sooner.
    #[arg(
        long = "retry-stream-hold-bytes",
        env = "HEADROOM_RETRY_STREAM_HOLD_BYTES",
        default_value_t = 2048
    )]
    pub retry_stream_hold_bytes: usize,

    /// Base delay for exponential backoff in milliseconds. Default `1000`.
    #[arg(
        long = "retry-base-delay-ms",
        env = "HEADROOM_RETRY_BASE_DELAY_MS",
        default_value_t = 1000
    )]
    pub retry_base_delay_ms: u64,

    /// Ceiling on backoff delay in milliseconds. Default `30000`.
    #[arg(
        long = "retry-max-delay-ms",
        env = "HEADROOM_RETRY_MAX_DELAY_MS",
        default_value_t = 30000
    )]
    pub retry_max_delay_ms: u64,

    /// Enable cost tracking for upstream requests. Default `true`.
    #[arg(
        long = "cost-tracking",
        env = "HEADROOM_COST_TRACKING_ENABLED",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub cost_tracking_enabled: bool,

    /// Budget limit in USD. `None` (no flag) = unlimited.
    #[arg(long = "budget-limit-usd", env = "HEADROOM_BUDGET_LIMIT_USD")]
    pub budget_limit_usd: Option<f64>,

    /// Budget aggregation period: "hourly", "daily", or "monthly".
    /// Default "daily".
    #[arg(
        long = "budget-period",
        env = "HEADROOM_BUDGET_PERIOD",
        default_value = "daily"
    )]
    pub budget_period: String,

    // ─── Compression tuning ──────────────────────────────────────────────
    /// Minimum token count before a message is eligible for compression.
    #[arg(
        long = "min-tokens-to-crush",
        env = "HEADROOM_MIN_TOKENS_TO_CRUSH",
        default_value_t = 200
    )]
    pub min_tokens_to_crush: usize,

    /// Max items to retain after SmartCrusher processing.
    #[arg(
        long = "max-items-after-crush",
        env = "HEADROOM_MAX_ITEMS_AFTER_CRUSH",
        default_value_t = 15
    )]
    pub max_items_after_crush: usize,

    /// Compression savings profile name.
    #[arg(
        long = "savings-profile",
        env = "HEADROOM_SAVINGS_PROFILE",
        default_value = "balanced"
    )]
    pub savings_profile: String,

    /// Target compression ratio (0.0 = auto).
    #[arg(
        long = "target-ratio",
        env = "HEADROOM_TARGET_RATIO",
        default_value_t = 0.0
    )]
    pub target_ratio: f64,

    /// Enable code-aware compressor for source code.
    #[arg(long = "code-aware", env = "HEADROOM_CODE_AWARE_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub code_aware_enabled: bool,

    /// Disable the Kompress ML compressor entirely.
    #[arg(long = "disable-kompress", env = "HEADROOM_DISABLE_KOMPRESS", default_value_t = true, action = clap::ArgAction::Set)]
    pub disable_kompress: bool,

    /// When Kompress is disabled, route fall-through to passthrough.
    #[arg(long = "disable-kompress-fallback", env = "HEADROOM_DISABLE_KOMPRESS_FALLBACK", default_value_t = true, action = clap::ArgAction::Set)]
    pub disable_kompress_fallback: bool,

    /// Disable Kompress for Anthropic provider only.
    #[arg(long = "disable-kompress-anthropic", env = "HEADROOM_DISABLE_KOMPRESS_ANTHROPIC", default_value_t = false, action = clap::ArgAction::Set)]
    pub disable_kompress_anthropic: bool,

    /// Disable Kompress for OpenAI provider only.
    #[arg(long = "disable-kompress-openai", env = "HEADROOM_DISABLE_KOMPRESS_OPENAI", default_value_t = false, action = clap::ArgAction::Set)]
    pub disable_kompress_openai: bool,

    /// Force all compressible content through Kompress.
    #[arg(long = "force-kompress-all", env = "HEADROOM_FORCE_KOMPRESS_ALL", default_value_t = false, action = clap::ArgAction::Set)]
    pub force_kompress_all: bool,

    /// Enable image token optimization.
    #[arg(long = "image-optimize", env = "HEADROOM_IMAGE_OPTIMIZE", default_value_t = true, action = clap::ArgAction::Set)]
    pub image_optimize: bool,

    /// Enable SmartCrusher compaction step.
    #[arg(long = "smart-crusher-compaction", env = "HEADROOM_SMART_CRUSHER_WITH_COMPACTION", default_value_t = true, action = clap::ArgAction::Set)]
    pub smart_crusher_with_compaction: bool,

    /// Gate compression of user-role messages.
    ///
    /// **Not wired. Setting this changes nothing.** Verified 2026-08-17: the
    /// field is read only by the `agent-savings` CLI subcommand, never on a
    /// serving path. `content_router::Config` carries a field of the same name
    /// that only a `SavingsProfile` ever writes and nothing reads, and
    /// `live_zone::DispatchConfig` declares one that is neither read nor
    /// written. `skip_user_messages`, which the doc comments say this overrides,
    /// is itself only declared and defaulted. See [[features-on-but-inert]].
    #[arg(long = "compress-user-messages", env = "HEADROOM_COMPRESS_USER_MESSAGES", default_value_t = true, action = clap::ArgAction::Set)]
    pub compress_user_messages: bool,

    /// Gate compression of system-role messages.
    ///
    /// **Not wired. Setting this changes nothing.** Same three dead layers as
    /// `compress_user_messages` above.
    #[arg(long = "compress-system-messages", env = "HEADROOM_COMPRESS_SYSTEM_MESSAGES", default_value_t = true, action = clap::ArgAction::Set)]
    pub compress_system_messages: bool,

    /// Protect recent reads from compression.
    #[arg(long = "protect-recent", env = "HEADROOM_PROTECT_RECENT", default_value_t = false, action = clap::ArgAction::Set)]
    pub protect_recent: bool,

    /// Protect analysis context from compression.
    #[arg(long = "protect-analysis-context", env = "HEADROOM_PROTECT_ANALYSIS_CONTEXT", default_value_t = false, action = clap::ArgAction::Set)]
    pub protect_analysis_context: bool,

    /// Accuracy guard string for compression safety.
    #[arg(
        long = "accuracy-guard",
        env = "HEADROOM_ACCURACY_GUARD",
        default_value = ""
    )]
    pub accuracy_guard: String,

    /// Enable lossless-only compression mode.
    #[arg(long = "lossless", env = "HEADROOM_LOSSLESS", default_value_t = false, action = clap::ArgAction::Set)]
    pub lossless: bool,

    // ─── CCR extensions ──────────────────────────────────────────────────
    /// CCR: inject retrieval markers into compressed output.
    #[arg(long = "ccr-inject-marker", env = "HEADROOM_CCR_INJECT_MARKER", default_value_t = true, action = clap::ArgAction::Set)]
    pub ccr_inject_marker: bool,

    // `--ccr-inject-system-instructions`, `--fallback` and
    // `--fallback-provider` used to live here. Nothing ever read them: they
    // were parsed, copied into `Config`, and that was the end of it. A flag
    // that advertises a feature the binary does not have is worse than no
    // flag, so they are gone rather than defaulted off. Cross-provider
    // fallback needs a real implementation before it needs a switch.

    // ─── Tool filtering ──────────────────────────────────────────────────
    /// Comma-separated tool names to exclude from compression.
    ///
    /// Defaults to Python's `DEFAULT_EXCLUDE_TOOLS`: file and search results
    /// are what the model is most likely to need verbatim, and the
    /// `all_messages` path compresses without storing an original to retrieve,
    /// so a summarized file read cannot be undone. Pass `--exclude-tools ""`
    /// to compress them anyway.
    #[arg(
        long = "exclude-tools",
        env = "HEADROOM_EXCLUDE_TOOLS",
        default_value = headroom_core::tool_exclusion::DEFAULT_EXCLUDE_TOOLS_CSV
    )]
    pub exclude_tools: String,

    /// Comma-separated tool names whose results must not be lossy-compressed.
    #[arg(
        long = "protect-tool-results",
        env = "HEADROOM_PROTECT_TOOL_RESULTS",
        default_value = ""
    )]
    pub protect_tool_results: String,

    // ─── Read lifecycle ──────────────────────────────────────────────────
    /// Enable read lifecycle tracking.
    #[arg(long = "read-lifecycle", env = "HEADROOM_READ_LIFECYCLE", default_value_t = false, action = clap::ArgAction::Set)]
    pub read_lifecycle: bool,

    /// Enable read maturation (hold fresh reads out of prefix cache).
    #[arg(long = "read-maturation", env = "HEADROOM_READ_MATURATION", default_value_t = false, action = clap::ArgAction::Set)]
    pub read_maturation: bool,

    // ─── Infrastructure ──────────────────────────────────────────────────
    /// Stateless mode: disable filesystem writes.
    #[arg(long = "stateless", env = "HEADROOM_STATELESS", default_value_t = false, action = clap::ArgAction::Set)]
    pub stateless: bool,

    /// Proxy auth token for inbound requests.
    #[arg(long = "proxy-token", env = "HEADROOM_PROXY_TOKEN")]
    pub proxy_token: Option<String>,

    /// Offline mode: disable all outbound network egress.
    #[arg(long = "offline", env = "HEADROOM_OFFLINE", default_value_t = false, action = clap::ArgAction::Set)]
    pub offline: bool,

    // ─── Concurrency ─────────────────────────────────────────────────────
    /// Pre-upstream concurrency limit (semaphore).
    #[arg(
        long = "anthropic-pre-upstream-concurrency",
        env = "HEADROOM_ANTHROPIC_PRE_UPSTREAM_CONCURRENCY",
        default_value_t = 1000
    )]
    pub anthropic_pre_upstream_concurrency: usize,

    /// Compression worker threadpool size.
    #[arg(
        long = "compression-max-workers",
        env = "HEADROOM_COMPRESSION_MAX_WORKERS",
        default_value_t = 4
    )]
    pub compression_max_workers: usize,

    /// Azure AI Foundry upstream base URL for `/anthropic/v1/messages`
    /// requests (e.g. `https://{resource}.services.ai.azure.com/anthropic`).
    /// Matches the Python proxy's `ANTHROPIC_FOUNDRY_BASE_URL` handling
    /// (`headroom/providers/registry.py::resolve_api_overrides`). When
    /// unset, falls back to derivation from `--foundry-resource`, then
    /// to `--upstream`.
    ///
    /// Source priority: CLI flag → `ANTHROPIC_FOUNDRY_BASE_URL` env
    /// var → derived from `--foundry-resource` → none.
    #[arg(long = "foundry-base-url", env = "ANTHROPIC_FOUNDRY_BASE_URL")]
    pub foundry_base_url: Option<Url>,

    /// Azure AI Foundry resource name. When `--foundry-base-url` is
    /// unset, the Foundry upstream is derived as
    /// `https://{resource}.services.ai.azure.com/anthropic` — the same
    /// derivation Claude Code performs internally and that the Python
    /// side implements in `headroom/cli/wrap.py::_foundry_upstream_url`.
    ///
    /// Source priority: CLI flag → `ANTHROPIC_FOUNDRY_RESOURCE` env
    /// var → none.
    #[arg(long = "foundry-resource", env = "ANTHROPIC_FOUNDRY_RESOURCE")]
    pub foundry_resource: Option<String>,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| format!("invalid duration `{s}`: {e}"))
}

fn parse_rollout_channel(value: &str) -> Result<String, String> {
    value
        .parse::<RolloutChannel>()
        .map(|channel| channel.as_str().to_owned())
        .map_err(|_| {
            format!("unknown rollout channel `{value}` (valid: stable, beta, canary, dev)")
        })
}

fn parse_rollout_features(value: &str) -> Result<String, String> {
    let valid = feature_names();
    let unknown: Vec<_> = split_feature_names(value)
        .into_iter()
        .filter(|name| !valid.contains(name.as_str()))
        .collect();
    if unknown.is_empty() {
        Ok(value.to_owned())
    } else {
        Err(format!(
            "unknown rollout feature(s): {}; valid: {}",
            unknown.join(", "),
            valid.into_iter().collect::<Vec<_>>().join(", ")
        ))
    }
}

fn parse_bytes(s: &str) -> Result<u64, String> {
    s.parse::<bytesize::ByteSize>()
        .map(|b| b.as_u64())
        .map_err(|e| format!("invalid byte size `{s}`: {e}"))
}

/// Build the tool-pruning policy from the three comma-separated CLI/env flags.
/// Empty / whitespace-only entries are ignored; an absent flag is an empty set.
/// When all are `None`, the resulting policy `is_noop()` (passthrough).
fn parse_prune_policy(
    keep_tools: Option<&str>,
    drop_mcp: Option<&str>,
    drop_tools: Option<&str>,
) -> crate::cache_stabilization::tool_prune::PrunePolicy {
    fn split(s: Option<&str>) -> std::collections::BTreeSet<String> {
        s.into_iter()
            .flat_map(|raw| raw.split(','))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    }
    crate::cache_stabilization::tool_prune::PrunePolicy {
        keep_names: split(keep_tools),
        drop_mcp_servers: split(drop_mcp),
        drop_names: split(drop_tools),
    }
}

/// A model-to-upstream routing rule.
#[derive(Debug, Clone)]
pub struct ModelRoute {
    /// Model name prefix to match (exact match, or trailing `*` for prefix).
    pub model_prefix: String,
    /// Whether this is a prefix match (model_prefix ends with `*`).
    pub prefix_match: bool,
    /// Upstream URL to route matching requests to.
    pub upstream: Option<Url>,
    /// Whether to translate Anthropic format → OpenAI format.
    pub translate: bool,
    /// When set, route through `mimo run` subprocess instead of HTTP.
    /// The value is the model ID to pass to `mimo run -m`.
    pub mimo_run: Option<String>,
    /// When set, overrides the `model` field sent to the upstream after
    /// translation. Lets `model_prefix` be a discoverable id (e.g.
    /// `claude-codex-5.5`, satisfying Claude Code's gateway-discovery
    /// filter) while the real upstream model id (e.g. `gpt-5.5`) differs.
    pub target_model: Option<String>,
}

impl ModelRoute {
    pub fn matches(&self, model: &str) -> bool {
        if self.prefix_match {
            model.starts_with(self.model_prefix.trim_end_matches('*'))
        } else {
            model == self.model_prefix
        }
    }
}

/// Parse `MODEL_NAME=UPSTREAM_URL[:translate[:TARGET_MODEL_ID]]` or
/// `MODEL_NAME=mimo:MODEL_ID` into a `ModelRoute`. `TARGET_MODEL_ID` lets
/// `MODEL_NAME` be a discoverable id (e.g. `claude-codex-5.5`, to satisfy
/// Claude Code's gateway-discovery filter) while the real upstream model id
/// (e.g. `gpt-5.5`) differs.
/// Parse the `HEADROOM_BEDROCK_MODEL_MAP` operator override.
///
/// Comma-separated `name=target` pairs. Whitespace around each pair is
/// trimmed; blank or malformed (no `=`) entries are skipped. Mirrors
/// Python's `_parse_bedrock_model_overrides` (#1795) including its
/// `partition("=")` semantics — the first `=` splits, everything after
/// it (including further `=` chars) belongs to the value.
pub fn parse_bedrock_model_map(raw: Option<&str>) -> HashMap<String, String> {
    let mut overrides = HashMap::new();
    let Some(raw) = raw else {
        return overrides;
    };
    for pair in raw.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let Some((name, target)) = pair.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let target = target.trim();
        if !name.is_empty() && !target.is_empty() {
            overrides.insert(name.to_string(), target.to_string());
        }
    }
    overrides
}

/// Warn about routes that translate to OpenAI but never reach the agentic API.
///
/// The endpoint family is chosen by whether a route carries a `TARGET_MODEL_ID`,
/// not by the translate keyword:
///
/// - `URL:openai:TARGET` -> `/v1/responses` (or the ChatGPT Codex backend)
/// - `URL:openai`        -> `/v1/chat/completions`
///
/// A Codex setup needs the first. Writing the second is easy, looks right, and
/// fails later as an upstream auth or 404 error rather than a config error —
/// a ChatGPT bearer token is not accepted by `api.openai.com`. Warning at
/// startup turns a confusing runtime failure into an obvious one.
///
/// Only warns when Codex auth is actually configured, so operators legitimately
/// pointing a route at a chat-completions backend are not nagged.
pub fn warn_on_ambiguous_codex_routes(routes: &[ModelRoute], codex_auth_file: Option<&str>) {
    if codex_auth_file.is_none() {
        return;
    }
    for route in routes {
        if route.translate && route.target_model.is_none() {
            if let Some(upstream) = &route.upstream {
                if upstream.host_str() == Some("api.openai.com") {
                    tracing::warn!(
                        model = %route.model_prefix,
                        upstream = %upstream,
                        "model route translates to OpenAI but has no TARGET_MODEL_ID, so it \
                         routes to /v1/chat/completions, not the agentic Responses API. \
                         Codex auth is configured, which suggests this was meant to be \
                         '<url>:openai:<target-model-id>'."
                    );
                }
            }
        }
    }
}

/// Warn about routes that can never match because an earlier route covers them.
///
/// Route lookup takes the first match, so a second route sharing a name with an
/// earlier one — or any route already swallowed by an earlier `*` prefix — is
/// dead config. Nothing rejects it and nothing logs it: requests just quietly
/// go to the first route, which looks identical to the route working. Writing
/// three variants of one model under the same name is the easy version of this
/// mistake, and the symptom is that switching models does nothing.
pub fn warn_on_shadowed_routes(routes: &[ModelRoute]) {
    for (model, shadowed_by) in shadowed_routes(routes) {
        tracing::warn!(
            model = %model,
            shadowed_by = %shadowed_by,
            "model route is unreachable: an earlier route already matches this name, \
             and the first match wins. Requests for it go to the earlier route. \
             Give each route a distinct model name."
        );
    }
}

/// Pairs of `(unreachable route, the earlier route that swallows it)`.
fn shadowed_routes(routes: &[ModelRoute]) -> Vec<(&str, &str)> {
    routes
        .iter()
        .enumerate()
        .filter_map(|(i, route)| {
            let shadow = routes[..i]
                .iter()
                .find(|earlier| earlier.matches(&route.model_prefix))?;
            Some((route.model_prefix.as_str(), shadow.model_prefix.as_str()))
        })
        .collect()
}

/// Keywords that turn on Anthropic -> OpenAI format translation in a route spec.
///
/// `translate` is the original spelling and stays supported forever; `openai`
/// is the clearer synonym, since what the flag actually does is convert the
/// request into OpenAI's format. They are exact aliases — same parsing, same
/// behaviour.
///
/// Note the keyword only selects the *format*. Whether the request goes to the
/// agentic Responses API or to chat-completions is decided by the presence of
/// `TARGET_MODEL_ID`, not by which keyword is used. See
/// [`warn_on_ambiguous_codex_routes`].
const TRANSLATE_KEYWORDS: [&str; 2] = ["translate", "openai"];

fn parse_model_route(spec: &str) -> Result<ModelRoute, String> {
    let (model, rest) = spec.split_once('=').ok_or_else(|| {
        format!("expected MODEL=URL[:translate[:TARGET_ID]] or MODEL=mimo:ID, got: {spec}")
    })?;
    let model = model.trim().to_string();
    let prefix_match = model.ends_with('*');

    // Check for `mimo:MODEL_ID` special syntax
    if let Some(mimo_model) = rest.strip_prefix("mimo:") {
        return Ok(ModelRoute {
            model_prefix: model,
            prefix_match,
            upstream: None,
            translate: false,
            mimo_run: Some(mimo_model.trim().to_string()),
            target_model: None,
        });
    }

    // Check for a trailing `:<kw>` or `:<kw>:TARGET_ID` suffix, where `<kw>`
    // is any of TRANSLATE_KEYWORDS (but not `://` from the scheme).
    let mut parsed_suffix = None;
    for kw in TRANSLATE_KEYWORDS {
        let with_target = format!(":{kw}:");
        if let Some(idx) = rest.find(&with_target) {
            let target = rest[idx + with_target.len()..].trim();
            if target.is_empty() {
                return Err(format!(
                    "expected non-empty TARGET_MODEL_ID after :{kw}: in {spec}"
                ));
            }
            parsed_suffix = Some((&rest[..idx], true, Some(target.to_string())));
            break;
        }
        let bare = format!(":{kw}");
        if rest.ends_with(&bare) {
            parsed_suffix = Some((&rest[..rest.len() - bare.len()], true, None));
            break;
        }
    }
    let (url_str, translate, target_model) = parsed_suffix.unwrap_or((rest, false, None));

    let url_str = url_str.trim();
    let upstream: Url = url_str
        .parse()
        .map_err(|e| format!("invalid URL '{url_str}': {e}"))?;

    Ok(ModelRoute {
        model_prefix: model,
        prefix_match,
        upstream: Some(upstream),
        translate,
        mimo_run: None,
        target_model,
    })
}

/// Resolved configuration used by the running server.
#[derive(Debug, Clone)]
pub struct Config {
    /// Runtime rollout state resolved from CLI/env.
    pub rollout: RolloutSnapshot,
    pub listen: SocketAddr,
    pub upstream: Url,
    pub upstream_timeout: Duration,
    pub upstream_connect_timeout: Duration,
    /// Provider-only HTTP proxy for upstream calls. See the CLI arg of
    /// the same name; scoped to the provider HTTP client, never exported
    /// to the process environment.
    pub http_proxy: Option<String>,
    pub max_body_bytes: u64,
    pub log_level: String,
    pub rewrite_host: bool,
    pub graceful_shutdown_timeout: Duration,
    /// Master switch for the LLM compression interceptor. When `false`,
    /// the proxy is pure streaming passthrough and never buffers a body.
    pub compression: bool,
    /// Effective ceiling for compression-time body buffering.
    /// Inherits `max_body_bytes` when not overridden. Bodies larger
    /// than this still forward, just unchanged.
    pub compression_max_body_bytes: u64,
    /// Policy mode for compression on `/v1/messages`. PR-A1 lockdown:
    /// both `Off` and `LiveZone` result in byte-faithful passthrough;
    /// `LiveZone` additionally emits a `tracing::warn!` per request
    /// because the dispatcher isn't implemented yet (Phase B PR-B2
    /// fills this in).
    pub compression_mode: CompressionMode,
    /// Cross-turn verbatim de-dup post-pass over tool outputs.
    /// Off by default (parity with Python's
    /// `enable_cross_turn_dedup = False`). Only active when the
    /// compression pipeline itself runs (`compression_mode` is not
    /// `Off`).
    pub enable_cross_turn_dedup: bool,
    /// Inject Anthropic context-editing directives into `/v1/messages`.
    pub context_edit: bool,
    /// `clear_tool_uses`: keep this many most-recent tool results.
    pub context_edit_keep_tool_uses: u64,
    /// `clear_tool_uses`: input-token trigger threshold.
    pub context_edit_trigger_tokens: u64,
    /// `clear_tool_uses`: leave conversations shorter than this many messages
    /// alone, so short ones never pay the one-time entry fee.
    pub context_edit_min_messages: usize,
    /// `clear_tool_uses`: skip the strategy unless it clears at least this many
    /// tokens.
    pub context_edit_clear_at_least: Option<u64>,
    /// When `Some(n)`, set `clear_thinking` to keep `n` thinking turns,
    /// overriding the `keep: "all"` the client sends.
    pub context_edit_keep_thinking: Option<u64>,
    /// Tool-pruning (A4) policy, parsed once from `--prune-keep-tools` /
    /// `--prune-drop-mcp`. `is_noop()` when neither flag is set (passthrough).
    pub tool_prune_policy: crate::cache_stabilization::tool_prune::PrunePolicy,
    /// Cost-aware model routing (issue #1706). Disabled unless
    /// `HEADROOM_MODEL_ROUTER_ENABLED` is truthy and `HEADROOM_MODEL_ROUTES`
    /// parses. Distinct from `model_routes`, which selects an *upstream* by
    /// model name; this rewrites the model itself based on request size.
    pub model_router: crate::model_router::ModelRouterConfig,
    /// Whether the live-zone dispatcher derives `frozen_message_count`
    /// automatically from customer `cache_control` markers. PR-A4
    /// adds the derivation function (`compute_frozen_count`); Phase
    /// B's dispatcher consumes the resolved value here.
    pub cache_control_auto_frozen: CacheControlAutoFrozen,
    /// Phase F PR-F2.1 c3/6: gate per-auth-mode `CompressionPolicy`
    /// enforcement. `Disabled` until c6/6 flips the default.
    pub auth_mode_policy_enforcement: AuthModePolicyEnforcement,
    /// Whether to strip internal `x-headroom-*` headers from
    /// upstream-bound requests. PR-A5 default-on guard against
    /// fingerprinting / leakage of internal flags.
    pub strip_internal_headers: StripInternalHeaders,
    /// Session-sticky provider beta headers (parity port of the
    /// Python `SessionBetaTracker`, PR-A6). Default `enabled`.
    pub beta_header_sticky: BetaHeaderSticky,
    /// PR-C4: enable the `/v1/responses` streaming pipeline (SSE
    /// state-machine + telemetry tee). Default `true`.
    pub enable_responses_streaming: bool,
    /// PR-C4: enable the `/v1/conversations*` passthrough surface
    /// (per-route axum handlers with explicit instrumentation).
    /// Default `true`. Strictly an instrumentation switch — does
    /// NOT gate compression of conversation items (that's
    /// C5+/B-phase territory).
    pub enable_conversations_passthrough: bool,
    /// Enable batch API routes (Google/OpenAI batch). Default `false`.
    pub enable_batch_api: bool,
    /// PR-D1: enable the native Bedrock InvokeModel route. Default
    /// `true`. When disabled, the explicit Rust handlers are not
    /// mounted; operators relying on the Python LiteLLM converter
    /// keep their existing path.
    pub enable_bedrock_native: bool,
    /// Enable the Kompress ML prose compressor for `PlainText` live-zone
    /// blocks. Default `false` — it loads a ~261 MB cache-only model, so
    /// operators opt in. Mirrors the Python reference's `enable_kompress`.
    pub enable_kompress: bool,
    /// CTX-2: passive session capture on/off. Pure observer; default `false`.
    pub ctx_capture: bool,
    /// CTX-2: base dir for sessions/content DBs. `None` → default
    /// `<workspace>/ctx`. Only used when `ctx_capture`.
    pub ctx_store_dir: Option<std::path::PathBuf>,
    /// CTX-3: tool_result offload transform on/off. Default `false`.
    pub ctx_offload: bool,
    /// Freeze-replay: byte-identical prefix replay across turns. Default `false`.
    pub prefix_replay: bool,
    /// Tail `cache_control` breakpoints to place on `messages`. Default `1`.
    pub cache_tail_breakpoints: u8,
    /// Drop the client's `system` breakpoints. Default `false`.
    pub strip_system_cache_breakpoints: bool,
    /// B2: replay last turn's tool order, appending new tools at the end.
    /// Default `true`.
    pub cache_stable_tool_order: bool,
    /// B1: pin `cache_control.ttl` to `1h`. Non-PAYG only. Default `false`.
    pub force_1h_cache_ttl: bool,
    /// 1h on the tools and system prefix, 5m on the message tail. Takes
    /// precedence over `force_1h_cache_ttl`; see
    /// [`crate::cache_stabilization::cache_ttl::tail_5m_prefix_1h`].
    pub cache_tail_breakpoint: bool,
    pub split_cache_ttl: bool,
    /// Hold each conversation's working-directory line in `system` still, and
    /// state the live one at the message tail. See
    /// [`crate::cache_stabilization::working_dir`].
    pub hold_working_directory: bool,
    /// Where forwarded prefixes are persisted so they survive a restart. Empty
    /// keeps them in memory only.
    pub replay_store_dir: String,
    /// CTX-3: min serialized byte length for a block to be offloaded.
    pub ctx_offload_min_bytes: usize,
    /// CTX-3: messages back from the tail before `exclude_tools` stops
    /// shielding a block from offload; `0` shields all of it.
    pub ctx_offload_stale_messages: usize,
    /// CTX-3: messages past that margin where a first conversion may skip the
    /// rebuild-boundary wait; `0` always waits.
    pub ctx_offload_stale_window: usize,
    /// CTX-3: CCR-store TTL (seconds) for offloaded originals.
    pub ctx_offload_ttl_seconds: u64,
    /// CTX-4: recall/resume injection on/off. Requires `ctx_capture`.
    pub ctx_inject: bool,
    /// CCR Phase 4: multi-turn context tracker on/off.
    pub ccr_context_tracking: bool,
    /// CCR Phase 4: proactive expansion on/off.
    pub ccr_proactive_expansion: bool,
    /// CCR Phase 4: max expansions appended to a turn.
    pub ccr_max_proactive_expansions: usize,
    /// PR-D1: AWS region used to sign Bedrock requests + (when no
    /// explicit endpoint is set) derive the Bedrock endpoint URL.
    pub bedrock_region: String,
    /// PR-D1: Bedrock endpoint base URL. `None` means
    /// "derive from region" (`https://bedrock-runtime.{region}.amazonaws.com`).
    pub bedrock_endpoint: Option<Url>,
    /// PR-D1: optional AWS profile name. When `None`, the default
    /// credential chain (env → `[default]` profile → IMDS) is used.
    pub aws_profile: Option<String>,
    /// PR-D2: validate prelude + message CRC32 on inbound Bedrock
    /// EventStream frames. Default `true`. Off only for debugging.
    pub bedrock_validate_eventstream_crc: bool,
    /// Operator override map for Bedrock model ids, parsed from the
    /// `HEADROOM_BEDROCK_MODEL_MAP` env var (`name=target,...`). Keyed
    /// by the model_id the client sends in the inbound path; the value
    /// is the effective model_id (or application-inference-profile ARN)
    /// the proxy forwards to Bedrock. An ARN value forces the converse
    /// route. Empty when unset — passthrough behaviour is unchanged.
    /// Ports Python's `_parse_bedrock_model_overrides` (#1795).
    pub bedrock_model_map: HashMap<String, String>,
    /// PR-D4: GCP Vertex region tag (e.g. `us-central1`). Surfaced
    /// in structured logs only — the actual upstream URL comes from
    /// `Config::upstream`. Operators set this so observability
    /// dashboards can group Vertex traffic by region.
    pub vertex_region: String,
    /// PR-D4: GCP ADC OAuth scope used when fetching the bearer
    /// token. Default `https://www.googleapis.com/auth/cloud-platform`.
    pub vertex_adc_scope: String,
    /// Local model routing: when `Some`, requests whose `model` field
    /// matches this string are translated to OpenAI format and forwarded
    /// to `local_upstream`.
    pub local_model: Option<String>,
    /// Local model upstream URL. Required when `local_model` is `Some`.
    pub local_upstream: Option<Url>,
    /// Additional model routes from `--extra-model-route` flags.
    /// Evaluated after `local_model` (exact match takes priority).
    pub model_routes: Vec<ModelRoute>,
    /// Path to Codex auth JSON file. When set, the proxy reads the
    /// access_token from this file and uses it as a Bearer token for
    /// OpenAI upstream requests. Re-read on each request for refresh.
    pub codex_auth_file: Option<String>,
    /// Proxy run mode: "token" (prioritize compression) or "cache"
    /// (prioritize provider prefix cache stability). Normalized via
    /// `modes::normalize_proxy_mode`.
    pub mode: String,
    /// Master switch for output-token shaping (verbosity steering,
    /// effort routing). Env-driven via HEADROOM_OUTPUT_SHAPER.
    pub output_shaper_enabled: bool,
    /// Verbosity steering level 0-4 (0 = off, 4 = minimum tokens).
    pub verbosity_level: i32,
    /// Effort value used on mechanical tool-result continuations.
    pub mechanical_effort: String,
    /// Shared ceiling on bytes one request may gain across all injection
    /// stages. See `injection_budget.rs`.
    pub max_injection_bytes: usize,
    /// Memory system: master switch. When false, no memory operations run.
    pub memory_enabled: bool,
    /// Memory injection mode: "auto_tail" (append to user message) or "tool" (model calls memory_search).
    pub memory_mode: String,
    /// Inject memory tool definitions into requests.
    pub memory_inject_tools: bool,
    /// Inject memory context into user messages.
    pub memory_inject_context: bool,
    /// Use Anthropic's native memory_20250818 tool instead of custom tools.
    pub memory_use_native_tool: bool,
    /// Number of memories to retrieve for context injection.
    pub memory_top_k: usize,
    /// Minimum similarity score for memory retrieval.
    pub memory_min_similarity: f64,
    /// Enable per-key rate limiting. Defaults to `true` in `from_args`
    /// (matching Python's `rate_limit_enabled: bool = True`); disable with
    /// `HEADROOM_RATE_LIMIT_ENABLED=0`.
    pub rate_limit_enabled: bool,
    /// Requests per minute per API key (0 = unlimited).
    pub rate_limit_rpm: u32,
    /// Tokens per minute per API key (0 = unlimited).
    pub rate_limit_tpm: u32,
    /// Enable semantic response caching. When `true`, identical
    /// non-streaming requests are served from an in-memory LRU.
    pub cache_enabled: bool,
    /// TTL for cached responses (seconds).
    pub cache_ttl_seconds: u64,
    /// Max entries in the semantic response cache (LRU eviction).
    pub cache_max_entries: usize,
    /// CCR: inject `headroom_retrieve` tool into LLM requests.
    pub ccr_inject_tool: bool,
    /// CCR: auto-handle retrieval tool calls server-side.
    pub ccr_handle_responses: bool,
    /// CCR: max retrieve-continue rounds per response.
    pub ccr_max_retrieval_rounds: usize,
    /// Retry upstream requests on transient errors.
    pub retry_enabled: bool,
    /// Max retry attempts per upstream call.
    pub retry_max_attempts: u32,
    pub retry_overload_max_attempts: u32,
    /// Bytes held back before a streamed response counts as committed.
    pub retry_stream_hold_bytes: usize,
    /// Base delay for exponential backoff (ms).
    pub retry_base_delay_ms: u64,
    /// Ceiling on backoff delay (ms).
    pub retry_max_delay_ms: u64,
    /// Enable cost tracking for upstream requests.
    pub cost_tracking_enabled: bool,
    /// Budget limit in USD (None = unlimited).
    pub budget_limit_usd: Option<f64>,
    /// Budget aggregation period ("hourly", "daily", "monthly").
    pub budget_period: String,
    // ─── Compression tuning ──────────────────────────────────────────────
    pub min_tokens_to_crush: usize,
    pub max_items_after_crush: usize,
    pub savings_profile: String,
    pub target_ratio: f64,
    pub code_aware_enabled: bool,
    pub disable_kompress: bool,
    pub disable_kompress_fallback: bool,
    pub disable_kompress_anthropic: bool,
    pub disable_kompress_openai: bool,
    pub force_kompress_all: bool,
    pub image_optimize: bool,
    pub smart_crusher_with_compaction: bool,
    pub compress_user_messages: bool,
    pub compress_system_messages: bool,
    pub protect_recent: bool,
    pub protect_analysis_context: bool,
    pub accuracy_guard: String,
    pub lossless: bool,
    // ─── CCR extensions ──────────────────────────────────────────────────
    pub ccr_inject_marker: bool,
    // ─── Tool filtering ──────────────────────────────────────────────────
    pub exclude_tools: Vec<String>,
    pub protect_tool_results: Vec<String>,
    // ─── Read lifecycle ──────────────────────────────────────────────────
    pub read_lifecycle: bool,
    pub read_maturation: bool,
    // ─── Infrastructure ──────────────────────────────────────────────────
    pub stateless: bool,
    pub proxy_token: Option<String>,
    pub offline: bool,
    // ─── Concurrency ─────────────────────────────────────────────────────
    pub anthropic_pre_upstream_concurrency: usize,
    pub compression_max_workers: usize,

    /// Azure AI Foundry upstream for `/anthropic/v1/messages`.
    /// Resolved from `--foundry-base-url` / `ANTHROPIC_FOUNDRY_BASE_URL`,
    /// or derived from `--foundry-resource` / `ANTHROPIC_FOUNDRY_RESOURCE`
    /// (`https://{resource}.services.ai.azure.com/anthropic`). `None`
    /// means the Foundry route forwards to `Config::upstream`.
    pub foundry_base_url: Option<Url>,
}

impl Config {
    pub fn from_cli(args: CliArgs) -> Self {
        let mut explicit_features = Vec::new();
        if args.enable_responses_streaming {
            explicit_features.push(Feature::OpenAiResponsesStreaming);
        }
        if args.enable_bedrock_native {
            explicit_features.push(Feature::NativeBedrock);
        }
        // Preserve the pre-rollout rollback controls as legacy disables. Both
        // features are stable defaults in the registry, so merely omitting a
        // false flag from `explicit_features` would turn it straight back on.
        let mut disabled_features = split_feature_names(&args.disable_features);
        if !args.enable_responses_streaming {
            disabled_features.push(Feature::OpenAiResponsesStreaming.spec().name.to_owned());
        }
        if !args.enable_bedrock_native {
            disabled_features.push(Feature::NativeBedrock.spec().name.to_owned());
        }
        let rollout = RolloutSnapshot::from_parts_with_explicit(
            &args.rollout_channel,
            &args.features,
            &disabled_features.join(","),
            args.unsafe_allow_unstable_features,
            &explicit_features,
        );
        let rewrite_host = if args.no_rewrite_host {
            false
        } else {
            args.rewrite_host
        };
        let compression_max_body_bytes = args
            .compression_max_body_bytes
            .unwrap_or(args.max_body_bytes);
        // CTX features live inside the interception (body-buffering) path,
        // which `compression` gates. Enabling any `--ctx-*` flag without
        // `--compression` would otherwise be a silent no-op, so the ctx
        // flags imply interception. With `--compression-mode` still `off`
        // this buffers and observes but mutates nothing beyond the ctx
        // transforms themselves.
        let compression =
            args.compression || args.ctx_capture || args.ctx_offload || args.ctx_inject;
        // An unset `--compression-mode` means "whatever the enabling
        // flags imply", not `off`. Turning interception on and leaving
        // the mode at `off` buffers every body and mutates nothing —
        // which is what the proxy did for its whole life so far. The
        // Python launcher always passed `--compression
        // --compression-mode all_messages` together; match that.
        // An explicit `off` still forces `off`, and a proxy started
        // with no flags at all stays `off`.
        let compression_mode = args.compression_mode.unwrap_or({
            if compression {
                CompressionMode::AllMessages
            } else {
                CompressionMode::Off
            }
        });
        Self {
            rollout: rollout.clone(),
            listen: args.listen,
            upstream: args.upstream,
            upstream_timeout: args.upstream_timeout,
            upstream_connect_timeout: args.upstream_connect_timeout,
            http_proxy: args.http_proxy,
            max_body_bytes: args.max_body_bytes,
            log_level: args.log_level,
            rewrite_host,
            graceful_shutdown_timeout: args.graceful_shutdown_timeout,
            compression,
            compression_max_body_bytes,
            compression_mode,
            enable_cross_turn_dedup: args.enable_cross_turn_dedup,
            context_edit: args.context_edit,
            context_edit_keep_tool_uses: args.context_edit_keep_tool_uses,
            context_edit_trigger_tokens: args.context_edit_trigger_tokens,
            context_edit_min_messages: args.context_edit_min_messages,
            context_edit_clear_at_least: args.context_edit_clear_at_least,
            context_edit_keep_thinking: args.context_edit_keep_thinking,
            tool_prune_policy: parse_prune_policy(
                args.prune_keep_tools.as_deref(),
                args.prune_drop_mcp.as_deref(),
                args.prune_drop_tools.as_deref(),
            ),
            model_router: crate::model_router::ModelRouterConfig::from_env(
                std::env::var("HEADROOM_MODEL_ROUTER_ENABLED")
                    .ok()
                    .as_deref(),
                std::env::var("HEADROOM_MODEL_ROUTES").ok().as_deref(),
            ),
            cache_control_auto_frozen: args.cache_control_auto_frozen,
            auth_mode_policy_enforcement: args.auth_mode_policy_enforcement,
            strip_internal_headers: args.strip_internal_headers,
            beta_header_sticky: args.beta_header_sticky,
            enable_responses_streaming: rollout.is_enabled(
                Feature::OpenAiResponsesStreaming,
                args.enable_responses_streaming,
            ),
            enable_conversations_passthrough: args.enable_conversations_passthrough,
            enable_batch_api: args.enable_batch_api,
            enable_bedrock_native: rollout
                .is_enabled(Feature::NativeBedrock, args.enable_bedrock_native),
            enable_kompress: args.enable_kompress,
            ctx_capture: args.ctx_capture,
            ctx_store_dir: args.ctx_store_dir,
            ctx_offload: args.ctx_offload,
            prefix_replay: args.prefix_replay,
            cache_tail_breakpoints: args.cache_tail_breakpoints,
            strip_system_cache_breakpoints: args.strip_system_cache_breakpoints,
            cache_stable_tool_order: args.cache_stable_tool_order,
            force_1h_cache_ttl: args.force_1h_cache_ttl,
            split_cache_ttl: args.split_cache_ttl,
            cache_tail_breakpoint: args.cache_tail_breakpoint,
            hold_working_directory: args.hold_working_directory,
            replay_store_dir: args.replay_store_dir.clone(),
            ctx_offload_min_bytes: args.ctx_offload_min_bytes,
            ctx_offload_stale_messages: args.ctx_offload_stale_messages,
            ctx_offload_stale_window: args.ctx_offload_stale_window,
            ctx_offload_ttl_seconds: args.ctx_offload_ttl_seconds,
            ctx_inject: args.ctx_inject,
            ccr_context_tracking: args.ccr_context_tracking,
            ccr_proactive_expansion: args.ccr_proactive_expansion,
            ccr_max_proactive_expansions: args.ccr_max_proactive_expansions,
            bedrock_region: args.bedrock_region,
            bedrock_endpoint: args.bedrock_endpoint,
            aws_profile: args.aws_profile,
            bedrock_validate_eventstream_crc: args.bedrock_validate_eventstream_crc,
            bedrock_model_map: {
                let map = parse_bedrock_model_map(
                    std::env::var("HEADROOM_BEDROCK_MODEL_MAP").ok().as_deref(),
                );
                if !map.is_empty() {
                    let mut names: Vec<&String> = map.keys().collect();
                    names.sort();
                    tracing::info!(
                        event = "bedrock_model_map_loaded",
                        count = map.len(),
                        names = ?names,
                        "loaded Bedrock model override(s) from HEADROOM_BEDROCK_MODEL_MAP"
                    );
                }
                map
            },
            vertex_region: args.vertex_region,
            vertex_adc_scope: args.vertex_adc_scope,
            local_model: args.local_model,
            local_upstream: args.local_upstream,
            model_routes: args
                .extra_model_routes
                .iter()
                .flat_map(|s| s.split(','))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .filter_map(|s| match parse_model_route(s) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        tracing::warn!(
                            event = "invalid_model_route",
                            spec = %s,
                            error = %e,
                            "skipping invalid --extra-model-route"
                        );
                        None
                    }
                })
                .collect(),
            codex_auth_file: args.codex_auth_file.or_else(|| {
                let home = std::env::var("HOME").ok()?;
                let p = std::path::PathBuf::from(home).join(".codex/auth.json");
                if p.exists() {
                    Some(p.to_string_lossy().into_owned())
                } else {
                    None
                }
            }),
            mode: crate::modes::normalize_proxy_mode(
                Some(&args.mode),
                crate::modes::PROXY_MODE_TOKEN,
            ),
            output_shaper_enabled: args.output_shaper_enabled,
            verbosity_level: args.verbosity_level.max(0).min(4),
            mechanical_effort: args.mechanical_effort,
            max_injection_bytes: args.max_injection_bytes,
            memory_enabled: args.memory_enabled,
            memory_mode: std::env::var("HEADROOM_MEMORY_MODE")
                .unwrap_or_else(|_| "auto_tail".to_string()),
            memory_inject_tools: std::env::var("HEADROOM_MEMORY_INJECT_TOOLS")
                .map(|v| v != "0" && v.to_lowercase() != "false")
                .unwrap_or(true),
            memory_inject_context: std::env::var("HEADROOM_MEMORY_INJECT_CONTEXT")
                .map(|v| v != "0" && v.to_lowercase() != "false")
                .unwrap_or(true),
            memory_use_native_tool: std::env::var("HEADROOM_MEMORY_USE_NATIVE_TOOL")
                .map(|v| v == "1" || v.to_lowercase() == "true")
                .unwrap_or(false),
            memory_top_k: std::env::var("HEADROOM_MEMORY_TOP_K")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            memory_min_similarity: std::env::var("HEADROOM_MEMORY_MIN_SIMILARITY")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.3),
            rate_limit_enabled: std::env::var("HEADROOM_RATE_LIMIT_ENABLED")
                .map(|v| v == "1" || v.to_lowercase() == "true")
                .unwrap_or(true),
            rate_limit_rpm: std::env::var("HEADROOM_RPM")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
            rate_limit_tpm: std::env::var("HEADROOM_TPM")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100000),
            cache_enabled: args.cache_enabled,
            cache_ttl_seconds: args.cache_ttl_seconds,
            cache_max_entries: args.cache_max_entries,
            ccr_inject_tool: args.ccr_inject_tool,
            ccr_handle_responses: args.ccr_handle_responses,
            ccr_max_retrieval_rounds: args.ccr_max_retrieval_rounds,
            retry_enabled: args.retry_enabled,
            retry_max_attempts: args.retry_max_attempts,
            retry_overload_max_attempts: args.retry_overload_max_attempts,
            retry_stream_hold_bytes: args.retry_stream_hold_bytes,
            retry_base_delay_ms: args.retry_base_delay_ms,
            retry_max_delay_ms: args.retry_max_delay_ms,
            cost_tracking_enabled: args.cost_tracking_enabled,
            budget_limit_usd: args.budget_limit_usd,
            budget_period: args.budget_period,
            min_tokens_to_crush: args.min_tokens_to_crush,
            max_items_after_crush: args.max_items_after_crush,
            savings_profile: args.savings_profile,
            target_ratio: args.target_ratio,
            code_aware_enabled: args.code_aware_enabled,
            disable_kompress: args.disable_kompress,
            disable_kompress_fallback: args.disable_kompress_fallback,
            disable_kompress_anthropic: args.disable_kompress_anthropic,
            disable_kompress_openai: args.disable_kompress_openai,
            force_kompress_all: args.force_kompress_all,
            image_optimize: args.image_optimize,
            smart_crusher_with_compaction: args.smart_crusher_with_compaction,
            compress_user_messages: args.compress_user_messages,
            compress_system_messages: args.compress_system_messages,
            protect_recent: args.protect_recent,
            protect_analysis_context: args.protect_analysis_context,
            accuracy_guard: args.accuracy_guard,
            lossless: args.lossless,
            ccr_inject_marker: args.ccr_inject_marker,
            exclude_tools: args
                .exclude_tools
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            protect_tool_results: args
                .protect_tool_results
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            read_lifecycle: args.read_lifecycle,
            read_maturation: args.read_maturation,
            stateless: args.stateless,
            proxy_token: args.proxy_token,
            offline: args.offline,
            anthropic_pre_upstream_concurrency: args.anthropic_pre_upstream_concurrency,
            compression_max_workers: args.compression_max_workers,
            foundry_base_url: crate::foundry::resolve_foundry_base_url(
                args.foundry_base_url,
                args.foundry_resource.as_deref(),
            ),
        }
    }

    /// Test/library helper. Compression off by default — match
    /// production-default behaviour so existing tests stay unchanged.
    pub fn for_test(upstream: Url) -> Self {
        Self {
            rollout: RolloutSnapshot::default(),
            listen: "127.0.0.1:0".parse().unwrap(),
            upstream,
            upstream_timeout: Duration::from_secs(60),
            upstream_connect_timeout: Duration::from_secs(5),
            http_proxy: None,
            max_body_bytes: 100 * 1024 * 1024,
            log_level: "warn".into(),
            rewrite_host: true,
            graceful_shutdown_timeout: Duration::from_secs(5),
            compression: false,
            compression_max_body_bytes: 100 * 1024 * 1024,
            compression_mode: CompressionMode::Off,
            // Production default: cross-turn dedup off (Python parity).
            // Tests opt in per-case.
            enable_cross_turn_dedup: false,
            context_edit: false,
            context_edit_keep_tool_uses: 6,
            context_edit_trigger_tokens: 60_000,
            context_edit_min_messages: 40,
            context_edit_clear_at_least: None,
            context_edit_keep_thinking: None,
            tool_prune_policy: Default::default(),
            model_router: Default::default(),
            // Match production default so the cache-control walker is
            // exercised under test without per-test opt-in.
            cache_control_auto_frozen: CacheControlAutoFrozen::Enabled,
            // F2.1 c5/5: enforcement is ON by default in production
            // (Config::from_cli inherits the CliArgs default which is
            // `Enabled`). For tests, we keep `Disabled` so the
            // existing PAYG-shaped test expectations stay green
            // without per-test opt-out — F2.1's regression tests
            // opt-IN to enforcement explicitly. If you're writing a
            // new test that needs to exercise the enforcement-on
            // path, set this field to `Enabled` in your test setup.
            auth_mode_policy_enforcement: AuthModePolicyEnforcement::Disabled,
            // Production default: strip internal `x-headroom-*` headers
            // from upstream-bound requests. Tests opt out per-case via
            // `start_proxy_with`.
            strip_internal_headers: StripInternalHeaders::Enabled,
            // Production default: sticky beta-header union per
            // conversation (Python-parity). Tests opt out per-case.
            beta_header_sticky: BetaHeaderSticky::Enabled,
            // PR-C4: streaming pipeline + conversations passthrough
            // both default-on so tests exercise the same paths
            // production traffic will hit.
            enable_responses_streaming: true,
            enable_conversations_passthrough: true,
            enable_batch_api: false,
            // PR-D1: bedrock route default-on so tests exercise
            // it without per-test opt-in. Tests that set
            // `bedrock_endpoint` to a wiremock URL get the full
            // sign-and-forward path.
            enable_bedrock_native: true,
            // Kompress off by default in tests (and production): operators
            // opt in to the ~261 MB model. Tests that exercise the PlainText
            // path enable it explicitly via `set_kompress_enabled`.
            enable_kompress: false,
            // CTX-2 passive capture off by default (production + tests).
            ctx_capture: false,
            ctx_store_dir: None,
            ctx_offload: false,
            prefix_replay: false,
            cache_tail_breakpoints: 1,
            strip_system_cache_breakpoints: false,
            // B2 off in the test default so existing request-path tests keep
            // asserting byte-identical tool arrays; production defaults to on.
            cache_stable_tool_order: false,
            force_1h_cache_ttl: false,
            split_cache_ttl: false,
            cache_tail_breakpoint: false,
            hold_working_directory: false,
            replay_store_dir: String::new(),
            ctx_offload_min_bytes: 50_000,
            ctx_offload_stale_messages: 0,
            ctx_offload_stale_window: 0,
            ctx_offload_ttl_seconds: 604_800,
            ctx_inject: false,
            ccr_context_tracking: true,
            ccr_proactive_expansion: true,
            ccr_max_proactive_expansions: 2,
            bedrock_region: "us-east-1".to_string(),
            bedrock_endpoint: None,
            aws_profile: None,
            // PR-D2: production default — validate every CRC. Tests
            // that exercise corruption paths flip this off per-case.
            bedrock_validate_eventstream_crc: true,
            bedrock_model_map: HashMap::new(),
            // PR-D4: default Vertex region (used for log tagging
            // only; the upstream URL is `upstream`).
            vertex_region: "us-central1".to_string(),
            vertex_adc_scope: "https://www.googleapis.com/auth/cloud-platform".to_string(),
            local_model: None,
            local_upstream: None,
            model_routes: Vec::new(),
            codex_auth_file: None,
            mode: crate::modes::PROXY_MODE_TOKEN.to_string(),
            output_shaper_enabled: false,
            verbosity_level: 2,
            mechanical_effort: "low".to_string(),
            max_injection_bytes: crate::injection_budget::DEFAULT_MAX_INJECTION_BYTES,
            memory_enabled: false,
            memory_mode: "auto_tail".to_string(),
            memory_inject_tools: true,
            memory_inject_context: true,
            memory_use_native_tool: false,
            memory_top_k: 10,
            memory_min_similarity: 0.3,
            rate_limit_enabled: false,
            rate_limit_rpm: 0,
            rate_limit_tpm: 0,
            cache_enabled: true,
            cache_ttl_seconds: 3600,
            cache_max_entries: 1000,
            ccr_inject_tool: true,
            ccr_handle_responses: true,
            ccr_max_retrieval_rounds: 8,
            retry_enabled: true,
            retry_max_attempts: 3,
            retry_overload_max_attempts: 6,
            retry_stream_hold_bytes: 2048,
            retry_base_delay_ms: 1000,
            retry_max_delay_ms: 30000,
            cost_tracking_enabled: true,
            budget_limit_usd: None,
            budget_period: "daily".to_string(),
            min_tokens_to_crush: 200,
            max_items_after_crush: 15,
            savings_profile: "balanced".to_string(),
            target_ratio: 0.0,
            code_aware_enabled: false,
            disable_kompress: true,
            disable_kompress_fallback: true,
            disable_kompress_anthropic: false,
            disable_kompress_openai: false,
            force_kompress_all: false,
            image_optimize: true,
            smart_crusher_with_compaction: true,
            compress_user_messages: true,
            compress_system_messages: true,
            protect_recent: false,
            protect_analysis_context: false,
            accuracy_guard: String::new(),
            lossless: false,
            ccr_inject_marker: true,
            exclude_tools: Vec::new(),
            protect_tool_results: Vec::new(),
            read_lifecycle: false,
            read_maturation: false,
            stateless: false,
            proxy_token: None,
            offline: false,
            anthropic_pre_upstream_concurrency: 1000,
            compression_max_workers: 4,
            // Azure AI Foundry: no dedicated upstream by default —
            // the `/anthropic/v1/messages` route falls back to
            // `upstream`. Foundry tests set this to a wiremock URL.
            foundry_base_url: None,
        }
    }
}

#[cfg(test)]
mod prune_policy_tests {
    use super::parse_prune_policy;

    #[test]
    fn both_none_is_noop() {
        let p = parse_prune_policy(None, None, None);
        assert!(p.is_noop());
    }

    #[test]
    fn parses_comma_list_trimming_and_dropping_empties() {
        let p = parse_prune_policy(Some(" Read , Bash ,, "), None, None);
        assert_eq!(p.keep_names.len(), 2);
        assert!(p.keep_names.contains("Read"));
        assert!(p.keep_names.contains("Bash"));
        assert!(p.drop_mcp_servers.is_empty());
    }

    #[test]
    fn parses_drop_mcp_servers() {
        let p = parse_prune_policy(None, Some("chrome,tmux"), None);
        assert!(p.keep_names.is_empty());
        assert_eq!(p.drop_mcp_servers.len(), 2);
        assert!(p.drop_mcp_servers.contains("chrome"));
    }

    #[test]
    fn parses_drop_tools_names() {
        let p = parse_prune_policy(
            None,
            None,
            Some(" ListMcpResourcesTool ,,ReadMcpResourceTool"),
        );
        assert!(p.keep_names.is_empty());
        assert!(p.drop_mcp_servers.is_empty());
        assert_eq!(p.drop_names.len(), 2);
        assert!(p.drop_names.contains("ListMcpResourcesTool"));
        assert!(p.drop_names.contains("ReadMcpResourceTool"));
        assert!(!p.is_noop());
    }

    #[test]
    fn drop_tools_alone_is_not_noop_but_absent_is() {
        assert!(!parse_prune_policy(None, None, Some("WebSearch")).is_noop());
        assert!(parse_prune_policy(None, None, Some(" , ")).is_noop());
    }
}

#[cfg(test)]
mod bedrock_model_map_tests {
    use super::parse_bedrock_model_map;

    #[test]
    fn none_and_empty_yield_empty() {
        assert!(parse_bedrock_model_map(None).is_empty());
        assert!(parse_bedrock_model_map(Some("")).is_empty());
        assert!(parse_bedrock_model_map(Some("   ")).is_empty());
    }

    #[test]
    fn single_pair() {
        let arn = "arn:aws:bedrock:ap-southeast-1:1:application-inference-profile/x57j1esjrt66";
        let m = parse_bedrock_model_map(Some(&format!("claude-sonnet-5={arn}")));
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("claude-sonnet-5").unwrap(), arn);
    }

    #[test]
    fn multiple_pairs_and_whitespace() {
        let m = parse_bedrock_model_map(Some(" claude-sonnet-5=arn:a , claude-opus-4-8=arn:b "));
        assert_eq!(m.get("claude-sonnet-5").unwrap(), "arn:a");
        assert_eq!(m.get("claude-opus-4-8").unwrap(), "arn:b");
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn skips_malformed_entries() {
        // Missing "=" and blank/no-name segments are skipped; valid pairs survive.
        let m = parse_bedrock_model_map(Some("garbage,,claude-sonnet-5=arn:a,=noname"));
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("claude-sonnet-5").unwrap(), "arn:a");
    }

    #[test]
    fn first_equals_wins_partition_semantics() {
        // partition("=") — everything after the first `=` is the value.
        let m = parse_bedrock_model_map(Some("name=arn:aws:foo=bar"));
        assert_eq!(m.get("name").unwrap(), "arn:aws:foo=bar");
    }
}

#[cfg(test)]
mod model_route_tests {
    use super::*;

    #[test]
    fn parse_exact_match() {
        let r = parse_model_route("MiMo-V2.5=https://api.xiaomimimo.com/anthropic").unwrap();
        assert_eq!(r.model_prefix, "MiMo-V2.5");
        assert!(!r.prefix_match);
        assert!(!r.translate);
        assert!(r.matches("MiMo-V2.5"));
        assert!(!r.matches("MiMo-V2.5-extra"));
    }

    #[test]
    fn parse_prefix_match_with_translate() {
        let r = parse_model_route("codex-*=https://api.openai.com/v1:translate").unwrap();
        assert_eq!(r.model_prefix, "codex-*");
        assert!(r.prefix_match);
        assert!(r.translate);
        assert!(r.target_model.is_none());
        assert!(r.matches("codex-5.5"));
        assert!(r.matches("codex-5.4"));
        assert!(!r.matches("gpt-4o"));
    }

    #[test]
    fn parse_translate_with_target_model() {
        let r = parse_model_route("claude-codex-5.5=https://api.openai.com/v1:translate:gpt-5.5")
            .unwrap();
        assert_eq!(r.model_prefix, "claude-codex-5.5");
        assert!(r.translate);
        assert_eq!(r.target_model.as_deref(), Some("gpt-5.5"));
        assert!(r.matches("claude-codex-5.5"));
    }

    #[test]
    fn parse_comma_separated_routes() {
        let input = "MiMo-V2.5=https://api.xiaomimimo.com/anthropic,codex-*=https://api.openai.com/v1:translate";
        let routes: Vec<ModelRoute> = input
            .split(',')
            .map(|s| parse_model_route(s.trim()).unwrap())
            .collect();
        assert_eq!(routes.len(), 2);
        assert!(routes[0].matches("MiMo-V2.5"));
        assert!(routes[1].matches("codex-5.5"));
    }

    #[test]
    fn parse_invalid_format() {
        assert!(parse_model_route("no-equals-sign").is_err());
        assert!(parse_model_route("model=not-a-url").is_err());
        assert!(parse_model_route("model=https://api.openai.com/v1:translate:").is_err());
    }
}

#[cfg(test)]
mod ctx_implies_interception_tests {
    use super::*;
    use clap::Parser;

    fn cfg(extra: &[&str]) -> Config {
        let mut argv = vec!["headroom-proxy", "--upstream", "https://api.anthropic.com"];
        argv.extend_from_slice(extra);
        Config::from_cli(CliArgs::try_parse_from(argv).unwrap())
    }

    /// The default exclude list has to survive the trip from the constant
    /// through clap and the CSV split, or file reads get lossily compressed
    /// with nothing stored to retrieve them from.
    #[test]
    fn exclude_tools_defaults_to_the_python_list() {
        let c = cfg(&[]);
        for tool in [
            "Read",
            "Glob",
            "Grep",
            "Write",
            "Edit",
            "WebSearch",
            "WebFetch",
        ] {
            assert!(
                c.exclude_tools.iter().any(|t| t == tool),
                "{tool} must be excluded from compression by default"
            );
        }
        assert!(
            headroom_core::tool_exclusion::is_tool_excluded(
                "Read",
                c.exclude_tools.iter().map(String::as_str)
            ),
            "the parsed default must actually match through is_tool_excluded"
        );
    }

    /// An operator who wants the old behaviour can still ask for it.
    #[test]
    fn exclude_tools_can_be_emptied_explicitly() {
        let c = cfg(&["--exclude-tools", ""]);
        assert!(c.exclude_tools.is_empty());
    }

    #[test]
    fn ctx_flags_force_interception_on() {
        for flag in [
            "--ctx-capture=true",
            "--ctx-offload=true",
            "--ctx-inject=true",
        ] {
            let c = cfg(&[flag]);
            assert!(c.compression, "{flag} must imply interception");
            // Interception on with an unset mode resolves to
            // all_messages — buffering every body and then compressing
            // nothing was the bug this replaces.
            assert!(
                matches!(c.compression_mode, CompressionMode::AllMessages),
                "{flag} must resolve the mode to all_messages, got {:?}",
                c.compression_mode
            );
        }
    }

    #[test]
    fn no_ctx_flags_keeps_interception_off() {
        assert!(!cfg(&[]).compression);
        assert!(!cfg(&["--ctx-capture=false"]).compression);
    }

    #[test]
    fn bare_args_resolve_mode_off() {
        assert!(matches!(cfg(&[]).compression_mode, CompressionMode::Off));
    }

    #[test]
    fn users_ctx_flag_set_resolves_to_all_messages() {
        let c = cfg(&[
            "--ctx-capture=true",
            "--ctx-offload=true",
            "--ctx-inject=true",
        ]);
        assert!(c.compression);
        assert!(matches!(c.compression_mode, CompressionMode::AllMessages));
    }

    #[test]
    fn explicit_off_wins_over_ctx_flags() {
        let c = cfg(&[
            "--ctx-capture=true",
            "--ctx-offload=true",
            "--ctx-inject=true",
            "--compression-mode",
            "off",
        ]);
        assert!(c.compression, "ctx flags still turn interception on");
        assert!(
            matches!(c.compression_mode, CompressionMode::Off),
            "an explicit --compression-mode off must never be overridden"
        );
    }

    #[test]
    fn explicit_mode_survives_resolution() {
        let c = cfg(&["--compression", "--compression-mode", "live_zone"]);
        assert!(matches!(c.compression_mode, CompressionMode::LiveZone));
    }
}

#[cfg(test)]
mod model_route_keyword_tests {
    use super::*;

    fn route(spec: &str) -> ModelRoute {
        parse_model_route(spec).expect("spec parses")
    }

    /// `openai` is an exact alias of `translate` — same parse, same behaviour.
    #[test]
    fn openai_and_translate_parse_identically() {
        let url = "https://api.openai.com/v1";
        for (a, b) in [
            (format!("m={url}:translate"), format!("m={url}:openai")),
            (
                format!("m={url}:translate:gpt-5.6-terra"),
                format!("m={url}:openai:gpt-5.6-terra"),
            ),
        ] {
            let (left, right) = (route(&a), route(&b));
            assert_eq!(left.translate, right.translate);
            assert_eq!(left.target_model, right.target_model);
            assert_eq!(left.upstream, right.upstream);
        }
    }

    #[test]
    fn the_alias_carries_the_target_model() {
        let r = route("claude-codex-terra=https://api.openai.com/v1:openai:gpt-5.6-terra");
        assert!(r.translate);
        assert_eq!(r.target_model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(r.model_prefix, "claude-codex-terra");
    }

    /// The keyword must not be picked up out of the URL itself — `openai`
    /// appears in `api.openai.com`, but not preceded by a colon.
    #[test]
    fn a_host_containing_the_keyword_is_not_mistaken_for_it() {
        let r = route("m=https://api.openai.com/v1");
        assert!(!r.translate, "plain URL must not enable translation");
        assert_eq!(r.target_model, None);
        assert_eq!(r.upstream.unwrap().as_str(), "https://api.openai.com/v1");
    }

    #[test]
    fn an_empty_target_is_rejected_for_either_keyword() {
        for spec in [
            "m=https://api.openai.com/v1:translate:",
            "m=https://api.openai.com/v1:openai:",
        ] {
            assert!(
                parse_model_route(spec).is_err(),
                "{spec} should be rejected"
            );
        }
    }

    /// The original spelling must keep working — existing configs depend on it.
    #[test]
    fn the_original_translate_spelling_still_works() {
        let r = route("m=https://api.openai.com/v1:translate:gpt-5.6-luna");
        assert!(r.translate);
        assert_eq!(r.target_model.as_deref(), Some("gpt-5.6-luna"));
    }
}

#[cfg(test)]
mod shadowed_route_tests {
    use super::*;

    fn shadowed(specs: &[&str]) -> Vec<(String, String)> {
        let routes: Vec<ModelRoute> = specs
            .iter()
            .map(|s| parse_model_route(s).expect("spec parses"))
            .collect();
        shadowed_routes(&routes)
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn distinct_names_are_all_reachable() {
        assert!(shadowed(&[
            "claude-codex-5.6=https://api.openai.com/:openai:gpt-5.6-luna",
            "claude-codex-5.6-terra=https://api.openai.com/:openai:gpt-5.6-terra",
            "claude-codex-5.6-sol=https://api.openai.com/:openai:gpt-5.6-sol",
        ])
        .is_empty());
    }

    /// The mistake this exists to catch: three variants of one model written
    /// under one name, where only the first is ever reachable.
    #[test]
    fn a_repeated_name_reports_every_later_copy() {
        let hits = shadowed(&[
            "claude-codex-5.6=https://api.openai.com/:openai:gpt-5.6-luna",
            "claude-codex-5.6=https://api.openai.com/:openai:gpt-5.6-terra",
            "claude-codex-5.6=https://api.openai.com/:openai:gpt-5.6-sol",
        ]);
        assert_eq!(
            hits,
            vec![
                ("claude-codex-5.6".into(), "claude-codex-5.6".into()),
                ("claude-codex-5.6".into(), "claude-codex-5.6".into()),
            ]
        );
    }

    #[test]
    fn an_earlier_prefix_route_swallows_a_later_exact_one() {
        let hits = shadowed(&[
            "claude-codex-*=https://api.openai.com/:openai:gpt-5.6-luna",
            "claude-codex-5.6-sol=https://api.openai.com/:openai:gpt-5.6-sol",
        ]);
        assert_eq!(
            hits,
            vec![("claude-codex-5.6-sol".into(), "claude-codex-*".into())]
        );
    }

    /// Order is what makes a route dead, so the specific-first arrangement of
    /// the same two routes must stay quiet.
    #[test]
    fn the_specific_route_listed_first_is_fine() {
        assert!(shadowed(&[
            "claude-codex-5.6-sol=https://api.openai.com/:openai:gpt-5.6-sol",
            "claude-codex-*=https://api.openai.com/:openai:gpt-5.6-luna",
        ])
        .is_empty());
    }
}

#[cfg(test)]
mod ambiguous_codex_route_tests {
    use super::*;

    fn routes(spec: &str) -> Vec<ModelRoute> {
        vec![parse_model_route(spec).expect("spec parses")]
    }

    /// The warning only fires for the genuinely ambiguous shape: translating to
    /// OpenAI, no target model, Codex auth configured. Everything else is a
    /// legitimate configuration and must stay quiet.
    ///
    /// `tracing` output is not captured here, so these assert the predicate the
    /// function branches on rather than the emitted line.
    fn would_warn(spec: &str, codex_auth: Option<&str>) -> bool {
        if codex_auth.is_none() {
            return false;
        }
        routes(spec).iter().any(|r| {
            r.translate
                && r.target_model.is_none()
                && r.upstream
                    .as_ref()
                    .and_then(|u| u.host_str())
                    .is_some_and(|h| h == "api.openai.com")
        })
    }

    #[test]
    fn the_ambiguous_shape_warns() {
        assert!(would_warn(
            "m=https://api.openai.com/v1:openai",
            Some("/home/u/.codex/auth.json")
        ));
    }

    #[test]
    fn a_route_with_a_target_model_does_not_warn() {
        assert!(!would_warn(
            "m=https://api.openai.com/v1:openai:gpt-5.6-terra",
            Some("/home/u/.codex/auth.json")
        ));
    }

    /// Without Codex auth, a chat-completions route is a deliberate choice.
    #[test]
    fn without_codex_auth_nothing_warns() {
        assert!(!would_warn("m=https://api.openai.com/v1:openai", None));
    }

    /// A non-OpenAI upstream is someone else's gateway; not our business.
    #[test]
    fn a_third_party_upstream_does_not_warn() {
        assert!(!would_warn(
            "m=https://my-gateway.internal/v1:openai",
            Some("/home/u/.codex/auth.json")
        ));
    }

    /// Calling it must never panic, whatever the input.
    #[test]
    fn the_warning_helper_is_safe_to_call() {
        warn_on_ambiguous_codex_routes(&[], None);
        warn_on_ambiguous_codex_routes(
            &routes("m=https://api.openai.com/v1:openai"),
            Some("/home/u/.codex/auth.json"),
        );
    }
}

#[cfg(test)]
mod rollout_input_tests {
    use super::*;

    #[test]
    fn explicit_rollout_inputs_are_strict_and_diagnosable() {
        assert_eq!(parse_rollout_channel("CANARY").unwrap(), "canary");
        assert!(parse_rollout_channel("stabel")
            .unwrap_err()
            .contains("unknown rollout channel"));
        assert!(parse_rollout_features("native-bedrock").is_ok());
        let error = parse_rollout_features("native_bedrok").unwrap_err();
        assert!(error.contains("native_bedrok"));
        assert!(error.contains("native_bedrock"));
    }

    #[test]
    fn legacy_false_flags_remain_effective_rollout_disables() {
        let args = CliArgs::try_parse_from([
            "headroom-proxy",
            "--upstream",
            "http://127.0.0.1:9",
            "--enable-responses-streaming",
            "false",
            "--enable-bedrock-native",
            "false",
        ])
        .unwrap();

        let config = Config::from_cli(args);

        for feature in [Feature::OpenAiResponsesStreaming, Feature::NativeBedrock] {
            let decision = config.rollout.decision(feature);
            assert!(!decision.enabled);
            assert!(decision.disabled);
            assert_eq!(
                decision.reason,
                headroom_core::rollout::FeatureDecisionReason::Disabled
            );
        }
        assert!(!config.enable_responses_streaming);
        assert!(!config.enable_bedrock_native);
    }
}
