//! Cold-prefix cache-miss hook: what to rewrite when the prompt cache is dead.
//!
//! Rust port of `headroom/transforms/cold_prefix.py`.
//!
//! When the prefix cache has lapsed (idle since the last turn exceeded the
//! provider TTL), forwarding the byte-identical prefix buys nothing — the cache
//! is gone — so this is the safe moment for rewrites that would otherwise bust a
//! warm cache. What we rewrite depends on the model's reasoning shape:
//!
//! * **Plain-text reasoning (Kimi / GLM / DeepSeek-R1)** — reasoning is resent
//!   as billable text (`reasoning_content` field or inline `<think>`). On a cold
//!   turn we can DROP the old reasoning outright, not just Kompress it.
//! * **Encrypted reasoning (Claude / OpenAI Codex)** — the reasoning is an
//!   opaque server-side handle billed free/light; touching it saves nothing.
//!   Instead, on a cold turn, dedupe + drop superseded reads across the
//!   (now-unfreezable) prefix.
//!
//! This module is the *decision* surface (is-it-cold + which-shape); the
//! handlers apply the chosen rewrite. Nothing here panics or returns an error:
//! every entry point is fail-open, matching Python's "never raises".
//!
//! Cache note: dropping/deduping the prefix is cache-safe here **because the
//! cache is already dead** — nothing to bust. The one cost is the cold turn
//! re-caches a smaller prefix, which then benefits every subsequent warm turn
//! until the next lapse.

use std::sync::Arc;

use serde_json::Value;

use crate::ccr::CcrStore;

use super::cross_turn_dedup::dedup_messages;
use super::read_lifecycle::{ReadLifecycleConfig, ReadLifecycleManager};

/// Confidence margin, in seconds, added to the TTL before calling a prefix cold.
pub const DEFAULT_MARGIN_SECONDS: f64 = 60.0;

/// The state `is_cold_prefix` reads off a prompt-cache prefix tracker.
///
/// Python duck-types this on `PrefixCacheTracker` (`_idle_seconds_at_fetch` plus
/// an optional `resolved_cache_ttl_seconds()`); the trait is the Rust spelling
/// of the same two reads. A tracker that cannot supply a static TTL returns
/// `None` from [`resolved_cache_ttl_seconds`](PrefixCacheState::resolved_cache_ttl_seconds),
/// which mirrors Python's "attribute missing" branch.
pub trait PrefixCacheState {
    /// Idle gap captured at fetch. Python's missing/`None` attribute maps to `0.0`.
    fn idle_seconds_at_fetch(&self) -> f64;

    /// The tracker's static per-provider TTL guess, when it has one.
    fn resolved_cache_ttl_seconds(&self) -> Option<f64>;
}

/// True when the prompt-cache prefix has (confidently) lapsed.
///
/// Compares the idle gap captured at fetch to the provider cache TTL. Pass
/// `ttl_seconds` to use a KNOWN TTL (e.g. from [`anthropic_cache_ttl_seconds`],
/// which reads CC's actual 5m/1h config) — this is what makes cold detection
/// reliable. When `ttl_seconds` is `None` it falls back to the tracker's
/// [`PrefixCacheState::resolved_cache_ttl_seconds`] (a static per-provider
/// guess), reliable only where that default is documented-correct.
///
/// The margin makes us *confident* it is past TTL before treating it as cold —
/// we would rather miss a just-expired cache than rewrite a still-warm one (a
/// wrong TTL here is exactly what busts a warm cache). Returns `false` whenever
/// the inputs are unusable (conservative: never assume cold), which is the port
/// of Python's blanket `except: return False`.
pub fn is_cold_prefix(
    prefix_tracker: &dyn PrefixCacheState,
    margin_seconds: f64,
    ttl_seconds: Option<f64>,
) -> bool {
    let idle = prefix_tracker.idle_seconds_at_fetch();
    let ttl = match ttl_seconds {
        Some(t) => t,
        None => match prefix_tracker.resolved_cache_ttl_seconds() {
            Some(t) => t,
            // Python: no `resolved_cache_ttl_seconds` attribute → False.
            None => return false,
        },
    };
    // NaN compares false in both languages, so a garbage reading stays "warm".
    idle > ttl + margin_seconds
}

// ─── Anthropic cache-TTL detection ───────────────────────────────────────

const ANTHROPIC_FAMILIES: &[&str] = &["opus", "sonnet", "haiku", "fable", "mythos"];
const TRUTHY: &[&str] = &["1", "true", "yes", "on"];

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| TRUTHY.contains(&v.trim().to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn anthropic_family(model: &str) -> Option<&'static str> {
    let m = model.to_ascii_lowercase();
    ANTHROPIC_FAMILIES
        .iter()
        .copied()
        .find(|fam| m.contains(fam))
}

/// Collect explicit `cache_control.ttl` strings from a client request.
///
/// Anthropic prompt caching: `{"type":"ephemeral"}` is the 5m default;
/// `{"type":"ephemeral","ttl":"1h"}` (with the extended-cache-ttl beta) is 1h.
/// We read whatever Claude Code actually sent — the authoritative TTL signal.
fn cache_control_ttls(messages: &[Value], system: Option<&Value>) -> Vec<String> {
    let mut ttls: Vec<String> = Vec::new();

    fn push_ttl(ttls: &mut Vec<String>, container: &Value) {
        if let Some(ttl) = container
            .get("cache_control")
            .and_then(|cc| cc.get("ttl"))
            .and_then(Value::as_str)
        {
            if !ttls.iter().any(|t| t == ttl) {
                ttls.push(ttl.to_string());
            }
        }
    }

    fn scan_blocks(ttls: &mut Vec<String>, blocks: Option<&Value>) {
        let Some(Value::Array(items)) = blocks else {
            return;
        };
        for block in items {
            if block.is_object() {
                push_ttl(ttls, block);
            }
        }
    }

    scan_blocks(&mut ttls, system);
    for m in messages {
        if !m.is_object() {
            continue;
        }
        push_ttl(&mut ttls, m);
        scan_blocks(&mut ttls, m.get("content"));
    }
    ttls
}

/// The prompt-cache TTL Claude Code is actually using — not a guess.
///
/// Returns:
/// * `None` — prompt caching is OFF (`DISABLE_PROMPT_CACHING` or the per-model
///   `DISABLE_PROMPT_CACHING_<FAMILY>`). With no cache there is nothing to bust,
///   so the caller can recompact EVERY turn.
/// * `Some(3600)` — 1h caching (request `cache_control.ttl == "1h"`, or
///   `ENABLE_PROMPT_CACHING_1H`).
/// * `Some(300)` — 5m caching (default, `FORCE_PROMPT_CACHING_5M`, or
///   `cache_control.ttl == "5m"`).
///
/// Priority: the request's `cache_control.ttl` is authoritative for 1h-vs-5m (it
/// already reflects CC's env config + overage checks and needs no env sharing);
/// the env vars are the OFF signal + a fallback. Reading a wrong TTL is exactly
/// what would bust a warm cache, so this replaces the hardcoded 300s guess.
pub fn anthropic_cache_ttl_seconds(
    model: &str,
    messages: &[Value],
    system: Option<&Value>,
) -> Option<u32> {
    if env_truthy("DISABLE_PROMPT_CACHING") {
        return None;
    }
    if let Some(fam) = anthropic_family(model) {
        if env_truthy(&format!(
            "DISABLE_PROMPT_CACHING_{}",
            fam.to_ascii_uppercase()
        )) {
            return None;
        }
    }
    let ttls = cache_control_ttls(messages, system);
    if ttls.iter().any(|t| t == "1h") {
        return Some(3600);
    }
    if ttls.iter().any(|t| t == "5m") {
        return Some(300);
    }
    // cache_control present without an explicit ttl (or none parsed): env hint,
    // else Anthropic's 5m default. (We do NOT infer "off" from absence — a false
    // off would recompact a warm cache every turn.)
    if env_truthy("FORCE_PROMPT_CACHING_5M") {
        return Some(300);
    }
    if env_truthy("ENABLE_PROMPT_CACHING_1H") {
        return Some(3600);
    }
    Some(300)
}

// ─── Reasoning shape ─────────────────────────────────────────────────────

/// True if any assistant turn carries reasoning as PLAIN TEXT we can drop.
///
/// Two shapes: a Kimi-style `reasoning_content` field, or an inline
/// `<think>…</think>` span in *string* content (GLM / DeepSeek-R1). Encrypted
/// reasoning (Claude signature / OpenAI `encrypted_content`) never appears in
/// these forms, so this is `false` for those — which routes them to the dedupe /
/// superseded-read branch instead.
///
/// Content lists are deliberately not scanned for `<think>`, matching Python:
/// the inline shape only ever arrives as a bare string.
pub fn has_plaintext_reasoning(messages: &[Value]) -> bool {
    for m in messages {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(rc) = m.get("reasoning_content").and_then(Value::as_str) {
            if !rc.trim().is_empty() {
                return true;
            }
        }
        if let Some(c) = m.get("content").and_then(Value::as_str) {
            if c.contains("<think>") && c.contains("</think>") {
                return true;
            }
        }
    }
    false
}

// ─── Cold recompaction ───────────────────────────────────────────────────

/// Lossless whole-prefix recompaction for a confirmed-cold turn.
///
/// Runs the information-preserving rewrites over the *entire* conversation
/// (`frozen_message_count = 0`): stale/superseded read replacement followed by
/// whole-conversation verbatim dedupe. Used when the prompt cache is dead (idle
/// past TTL) and the byte-identical splice would preserve nothing. Lossless +
/// prefix-monotonic ⇒ deterministic per content ⇒ the recompacted prefix
/// re-caches and stays byte-stable on later warm turns.
///
/// Returns `(new_messages, transforms_applied)`. Fail-open by construction: both
/// underlying passes return their input unchanged when they cannot fold.
///
/// Divergence from Python: `cold_prefix.cold_recompact_messages` drives the
/// message-level `ContentRouter` in lossless + cross-turn-dedup mode. That
/// router entry point is not ported to Rust, so this composes the two passes it
/// runs that exist here — read lifecycle and cross-turn dedup — and omits the
/// per-block lossless folds and the router's `router:excluded:*` bookkeeping
/// tags. The dedup tag (`router:cross_turn_dedup:<n>`) keeps Python's spelling.
pub fn cold_recompact_messages(
    messages: &[Value],
    compression_store: Option<Arc<dyn CcrStore>>,
) -> (Vec<Value>, Vec<String>) {
    let lifecycle = ReadLifecycleManager::new(ReadLifecycleConfig::default(), compression_store);
    let result = lifecycle.apply(messages, 0);
    let mut out = if result.messages.is_empty() && !messages.is_empty() {
        messages.to_vec()
    } else {
        result.messages
    };
    let mut transforms = result.transforms_applied;

    let stats = dedup_messages(&mut out, 0);
    if !stats.error && stats.spans_folded > 0 {
        transforms.push(format!("router:cross_turn_dedup:{}", stats.spans_folded));
    }
    (out, transforms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Expected values below were measured by running the Python functions in the
    // repo `.venv` on these exact inputs.

    struct Tracker {
        idle: f64,
        ttl: Option<f64>,
    }

    impl Tracker {
        fn new(idle: f64, ttl: f64) -> Self {
            Self {
                idle,
                ttl: Some(ttl),
            }
        }
    }

    impl PrefixCacheState for Tracker {
        fn idle_seconds_at_fetch(&self) -> f64 {
            self.idle
        }
        fn resolved_cache_ttl_seconds(&self) -> Option<f64> {
            self.ttl
        }
    }

    /// Python's bare `object()` tracker: no idle attribute, no TTL method.
    struct Bare;

    impl PrefixCacheState for Bare {
        fn idle_seconds_at_fetch(&self) -> f64 {
            0.0
        }
        fn resolved_cache_ttl_seconds(&self) -> Option<f64> {
            None
        }
    }

    #[test]
    fn cold_detection_matches_python() {
        let m = DEFAULT_MARGIN_SECONDS;
        assert!(is_cold_prefix(&Tracker::new(400.0, 300.0), m, None));
        assert!(!is_cold_prefix(&Tracker::new(350.0, 300.0), m, None));
        // Strictly greater: exactly ttl + margin is still warm.
        assert!(!is_cold_prefix(&Tracker::new(360.0, 300.0), m, None));
        assert!(is_cold_prefix(&Tracker::new(361.0, 300.0), m, None));
        // Back-to-back turn.
        assert!(!is_cold_prefix(&Tracker::new(10.0, 300.0), m, None));
        // No TTL source at all → conservative false.
        assert!(!is_cold_prefix(&Bare, m, None));
        // A zero margin only needs to clear the TTL itself.
        assert!(is_cold_prefix(&Tracker::new(301.0, 300.0), 0.0, None));
        // A garbage idle reading must not read as cold.
        assert!(!is_cold_prefix(&Tracker::new(f64::NAN, 300.0), m, None));
    }

    #[test]
    fn a_known_ttl_overrides_the_trackers_guess() {
        let m = DEFAULT_MARGIN_SECONDS;
        // The exact bug: at 400s idle the hardcoded 300s guess says "cold" and
        // would bust a warm 1h cache; the real 1h TTL says "warm".
        assert!(!is_cold_prefix(
            &Tracker::new(400.0, 300.0),
            m,
            Some(3600.0)
        ));
        assert!(is_cold_prefix(&Tracker::new(400.0, 300.0), m, None));
        assert!(is_cold_prefix(
            &Tracker::new(3700.0, 300.0),
            m,
            Some(3600.0)
        ));
        // With a known TTL the missing method no longer matters; idle defaults
        // to 0, so it reads warm.
        assert!(!is_cold_prefix(&Bare, m, Some(10.0)));
    }

    #[test]
    fn plaintext_reasoning_detection_matches_python() {
        assert!(has_plaintext_reasoning(&[json!({
            "role": "assistant", "reasoning_content": "abc"
        })]));
        assert!(has_plaintext_reasoning(&[json!({
            "role": "assistant", "content": "<think>x</think> y"
        })]));
        // Whitespace-only reasoning is nothing to drop.
        assert!(!has_plaintext_reasoning(&[json!({
            "role": "assistant", "reasoning_content": "   "
        })]));
        // An unterminated span does not count.
        assert!(!has_plaintext_reasoning(&[json!({
            "role": "assistant", "content": "<think>x"
        })]));
        // Only STRING content is scanned — a block list is the encrypted shape.
        assert!(!has_plaintext_reasoning(&[json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "<think>x</think>"}]
        })]));
        assert!(!has_plaintext_reasoning(&[json!({
            "role": "assistant", "content": "plain"
        })]));
        // Non-assistant turns are skipped entirely.
        assert!(!has_plaintext_reasoning(&[json!({
            "role": "user", "reasoning_content": "x"
        })]));
        assert!(!has_plaintext_reasoning(&[]));
    }

    fn msg_with_ttl(ttl: Option<&str>) -> Value {
        let cc = match ttl {
            Some(t) => json!({"type": "ephemeral", "ttl": t}),
            None => json!({"type": "ephemeral"}),
        };
        json!({"role": "user", "content": [{"type": "text", "text": "x", "cache_control": cc}]})
    }

    /// Env is process-global, so the env-driven cases share one test.
    #[test]
    fn cache_ttl_matches_python() {
        let m1h = [msg_with_ttl(Some("1h"))];
        let m5m = [msg_with_ttl(None)];
        let m5m_explicit = [msg_with_ttl(Some("5m"))];

        for v in [
            "DISABLE_PROMPT_CACHING",
            "DISABLE_PROMPT_CACHING_OPUS",
            "ENABLE_PROMPT_CACHING_1H",
            "FORCE_PROMPT_CACHING_5M",
        ] {
            std::env::remove_var(v);
        }

        // --- request-driven ---
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &m1h, None),
            Some(3600)
        );
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &m5m, None),
            Some(300)
        );
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &m5m_explicit, None),
            Some(300)
        );
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &[], None),
            Some(300)
        );
        // 1h wins when both TTLs appear in one request.
        let mixed = [json!({
            "role": "user",
            "content": [
                {"type": "text", "cache_control": {"type": "ephemeral", "ttl": "5m"}},
                {"type": "text", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ]
        })];
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &mixed, None),
            Some(3600)
        );
        // Message-level cache_control counts too.
        let msg_level = [json!({
            "role": "user",
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "content": "x"
        })];
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &msg_level, None),
            Some(3600)
        );
        // The system prompt is scanned as well.
        let system = json!([{"type": "text", "text": "s",
            "cache_control": {"type": "ephemeral", "ttl": "1h"}}]);
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &[], Some(&system)),
            Some(3600)
        );
        // A plain string system prompt has no blocks to scan.
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &[], Some(&json!("plain system"))),
            Some(300)
        );
        // Junk entries are skipped rather than fatal.
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &[json!("not a dict"), json!(5)], None),
            Some(300)
        );
        // A non-string ttl is ignored (Python checks isinstance(str)).
        let numeric_ttl = [json!({
            "role": "user",
            "content": [{"cache_control": {"type": "ephemeral", "ttl": 3600}}]
        })];
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &numeric_ttl, None),
            Some(300)
        );
        // Family matching is case-insensitive on the model id; unknown families
        // simply skip the per-model OFF check.
        assert_eq!(
            anthropic_cache_ttl_seconds("CLAUDE-OPUS-4-6", &[], None),
            Some(300)
        );
        assert_eq!(anthropic_cache_ttl_seconds("gpt-5", &[], None), Some(300));

        // --- env-driven ---
        std::env::set_var("DISABLE_PROMPT_CACHING", "1");
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &m5m, None),
            None
        );
        // OFF beats an explicit 1h in the request.
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &m1h, None),
            None
        );
        std::env::set_var("DISABLE_PROMPT_CACHING", "0");
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &[], None),
            Some(300)
        );
        // Truthiness is trimmed + case-folded.
        std::env::set_var("DISABLE_PROMPT_CACHING", " TRUE ");
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &[], None),
            None
        );
        std::env::remove_var("DISABLE_PROMPT_CACHING");

        std::env::set_var("DISABLE_PROMPT_CACHING_OPUS", "1");
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &[], None),
            None
        );
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &m1h, None),
            None
        );
        // Another family is unaffected.
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-sonnet-4-6", &[], None),
            Some(300)
        );
        std::env::remove_var("DISABLE_PROMPT_CACHING_OPUS");

        std::env::set_var("ENABLE_PROMPT_CACHING_1H", "1");
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &[], None),
            Some(3600)
        );
        // The request still wins over the env hint.
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &m5m_explicit, None),
            Some(300)
        );
        std::env::set_var("FORCE_PROMPT_CACHING_5M", "1");
        assert_eq!(
            anthropic_cache_ttl_seconds("claude-opus-4-6", &[], None),
            Some(300)
        );
        std::env::remove_var("ENABLE_PROMPT_CACHING_1H");
        std::env::remove_var("FORCE_PROMPT_CACHING_5M");
    }

    #[test]
    fn cold_recompaction_folds_a_repeated_read() {
        let big: String = (0..40)
            .map(|i| {
                format!("line {i} of the shared file content that repeats verbatim across turns\n")
            })
            .collect();
        let big = big.trim_end().to_string();
        let messages = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/a.py"}}]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": big}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t2", "name": "Read", "input": {"file_path": "/a.py"}}]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t2", "content": big}]}),
        ];

        let (out, transforms) = cold_recompact_messages(&messages, None);

        // Python on this input: transforms end with `router:cross_turn_dedup:1`
        // and the second tool_result collapses to a pointer at msg 1.
        assert_eq!(
            transforms.last().map(String::as_str),
            Some("router:cross_turn_dedup:1")
        );
        let folded = out[3]["content"][0]["content"].as_str().unwrap();
        assert!(
            folded.contains("same as msg 1"),
            "expected a pointer to the earlier copy, got: {folded}"
        );
        assert!(folded.len() < big.len());
        // The first copy is the reference target and stays verbatim.
        assert_eq!(out[1]["content"][0]["content"].as_str(), Some(big.as_str()));
        // Superseded-read replacement is off by default, so nothing else moved.
        assert_eq!(out.len(), messages.len());
    }

    #[test]
    fn cold_recompaction_leaves_unfoldable_messages_alone() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ];
        let (out, transforms) = cold_recompact_messages(&messages, None);
        assert_eq!(out, messages);
        assert!(transforms.is_empty());
    }

    #[test]
    fn cold_recompaction_handles_an_empty_conversation() {
        let (out, transforms) = cold_recompact_messages(&[], None);
        assert!(out.is_empty());
        assert!(transforms.is_empty());
    }
}
