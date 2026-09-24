//! Content-type dispatch to a compressor, with the result cache and the
//! offload entry point.
//!
//! Moved out of `live_zone.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

/// Per-block dispatch result — whether any compressor ran and what
/// it produced.
pub(super) enum DispatchResult {
    /// No compressor was applicable for this content type, or one ran and
    /// declined because it could not shrink the block — `declined_by` tells
    /// the two apart.
    NoOp {
        content_type: &'static str,
        declined_by: Option<&'static str>,
    },
    /// A compressor ran and produced a candidate replacement string.
    Compressed {
        strategy: &'static str,
        compressed: String,
    },
    /// A compressor ran and failed loudly. The error string is
    /// surfaced via the manifest; the proxy logs it.
    #[allow(dead_code)]
    Error {
        strategy: &'static str,
        error: String,
    },
}

/// Map `(text, content_type)` to the compressor result.
///
/// Per spec PR-B3:
///
/// - `JsonArray` (with `is_dict_array=true`) → SmartCrusher
/// - `BuildOutput` → LogCompressor
/// - `SearchResults` → SearchCompressor
/// - `GitDiff` → DiffCompressor
/// - `SourceCode` → CodeCompressor
/// - `PlainText` → Kompress (cache-only; passthrough when the model is
///   not in the local HF cache — never downloads on the dispatch thread)
/// - `Html` → no-op (no compressor)
///
/// Configuration for the compressor dispatch logic.
#[derive(Debug, Clone, Default)]
pub struct DispatchConfig {
    /// Target compression ratio for Kompress (None = auto).
    pub target_ratio: Option<f64>,
    /// Disable Kompress for specific providers.
    pub disable_kompress_per_provider: std::collections::HashMap<String, bool>,
    /// When Kompress is disabled, route to passthrough instead of fallback.
    pub disable_kompress_fallback: bool,
    /// Compress user-role messages (overrides skip_user_messages when Some(true)).
    pub compress_user_messages: Option<bool>,
    /// Compress system-role messages.
    pub compress_system_messages: Option<bool>,
    /// Tool names (`--exclude-tools`) whose `tool_result` blocks must not
    /// reach a lossy compressor. Matched by [`is_tool_excluded`], so exact
    /// names, globs and MCP spellings all work. Empty — the default — leaves
    /// dispatch exactly as it was before the flag was wired.
    pub exclude_tools: Vec<String>,
}

pub(super) fn dispatch_compressor(text: &str, content_type: ContentType) -> DispatchResult {
    dispatch_compressor_with_config(text, content_type, &DispatchConfig::default())
}

/// How long a memoised dispatch result stays valid. Matches Python's
/// `CompressionCache` default TTL.
pub(super) const DISPATCH_CACHE_TTL_SECS: u64 = 1800;

/// Process-global memo for [`dispatch_compressor_with_config`].
///
/// Python hangs its `CompressionCache` off the `ContentRouter` instance, but the
/// Rust live-zone entry points are free functions with no session object to hold
/// one, so the cache is process-global. That is sound here because what it
/// memoises is a *pure* function — see [`dispatch_compressor_with_config`].
pub(super) static DISPATCH_CACHE: OnceLock<crate::transforms::content_router::CompressionCache> =
    OnceLock::new();

pub(super) fn dispatch_cache() -> &'static crate::transforms::content_router::CompressionCache {
    DISPATCH_CACHE.get_or_init(|| {
        crate::transforms::content_router::CompressionCache::new(DISPATCH_CACHE_TTL_SECS)
    })
}

/// Key for a dispatch memo entry.
///
/// Every input that can change the output is included: the text, the content
/// type that selects the compressor, `target_ratio` — the only
/// [`DispatchConfig`] field this function reads (the rest are role gates applied
/// by callers, and `exclude_tools`, which the planner consumes before a block
/// ever reaches this cache) — and the global Kompress and CodeAware enable
/// flags. This extends Python's `hash((content, _runtime_target_ratio))`.
///
/// The Kompress flag has to be part of the key even though it is not an
/// argument: [`set_kompress_enabled`] flips it at runtime and it decides whether
/// `PlainText` compresses at all, so a key without it would serve a
/// Kompress-disabled result after Kompress was switched on. Same for
/// [`set_code_aware_enabled`] and `SourceCode`.
///
/// Like Python's, this is a 64-bit key with no stored copy of the input to
/// verify against, so a hash collision would serve another block's bytes. The
/// exposure is identical to the Python implementation's.
pub(super) fn dispatch_cache_key(
    text: &str,
    content_type: ContentType,
    target_ratio: Option<f64>,
) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    content_type.as_str().hash(&mut hasher);
    // `f64` is not `Hash`; its bit pattern is, and is exact for this purpose.
    target_ratio.map(f64::to_bits).hash(&mut hasher);
    KOMPRESS_ENABLED
        .load(std::sync::atomic::Ordering::Relaxed)
        .hash(&mut hasher);
    CODE_AWARE_ENABLED
        .load(std::sync::atomic::Ordering::Relaxed)
        .hash(&mut hasher);
    hasher.finish() as i64
}

/// Map a cached strategy name back to the `&'static str` [`DispatchResult`] needs.
///
/// Returns `None` for anything not produced by this function, in which case the
/// caller recompresses rather than inventing a leaked string.
pub(super) fn intern_dispatch_strategy(strategy: &str) -> Option<&'static str> {
    [
        STRATEGY_SMART_CRUSHER,
        STRATEGY_LOG_COMPRESSOR,
        STRATEGY_SEARCH_COMPRESSOR,
        STRATEGY_DIFF_COMPRESSOR,
        STRATEGY_CODE_COMPRESSOR,
        STRATEGY_CONFIG_LOSSLESS,
        STRATEGY_KOMPRESS,
    ]
    .into_iter()
    .find(|known| *known == strategy)
}

/// Dispatch `text` to the compressor for `content_type`, memoising the result.
///
/// This is a pure function of `(text, content_type, config.target_ratio)`, which
/// is what makes the process-global memo safe: identical inputs always produce
/// identical output, so a hit cannot be stale.
///
/// The memo deliberately sits *below* the accept/reject decision in
/// [`compress_one_block`], not above it. Python caches a compressed result
/// alongside the ratio it achieved, then has to re-check that ratio against the
/// live `min_ratio` on every hit — and demote the entry via `move_to_skip` when
/// the threshold tightens. Caching only the raw compression output sidesteps
/// that entirely: acceptance is recomputed from scratch each call, so a
/// threshold change is picked up immediately and no stale verdict can survive.
///
/// A `NoOp` is cached as a skip entry, which is what Python's skip tier is for —
/// content that compressors already declined once should not be re-run. `Error`
/// results are never cached, since a failure may be transient.
pub(super) fn dispatch_compressor_with_config(
    text: &str,
    content_type: ContentType,
    config: &DispatchConfig,
) -> DispatchResult {
    if text.is_empty() {
        return DispatchResult::NoOp {
            content_type: content_type.as_str(),
            declined_by: None,
        };
    }

    let cache = dispatch_cache();
    let key = dispatch_cache_key(text, content_type, config.target_ratio);
    match cache.get(key) {
        crate::transforms::content_router::CacheLookup::Skip => {
            return DispatchResult::NoOp {
                content_type: content_type.as_str(),
                declined_by: None,
            };
        }
        crate::transforms::content_router::CacheLookup::Hit {
            compressed,
            strategy,
            ..
        } => {
            // An unrecognised strategy means the entry predates a rename; fall
            // through and recompress rather than fabricate a static string.
            if let Some(strategy) = intern_dispatch_strategy(&strategy) {
                return DispatchResult::Compressed {
                    strategy,
                    compressed,
                };
            }
        }
        crate::transforms::content_router::CacheLookup::Miss => {}
    }

    let result = dispatch_compressor_uncached(text, content_type, config);
    match &result {
        DispatchResult::Compressed {
            strategy,
            compressed,
        } => {
            let ratio = compressed.len() as f64 / text.len() as f64;
            cache.put(key, compressed, ratio, strategy);
        }
        DispatchResult::NoOp { .. } => cache.mark_skip(key),
        // Transient by assumption — caching it would pin a failure for the TTL.
        DispatchResult::Error { .. } => {}
    }
    result
}

// `config` carries the target ratio, which only the Kompress arm reads.
#[cfg_attr(not(feature = "ml"), allow(unused_variables))]
pub(super) fn dispatch_compressor_uncached(
    text: &str,
    content_type: ContentType,
    config: &DispatchConfig,
) -> DispatchResult {
    if text.is_empty() {
        return DispatchResult::NoOp {
            content_type: content_type.as_str(),
            declined_by: None,
        };
    }

    let result = match content_type {
        ContentType::JsonArray => {
            // The detector classifies arrays-of-scalars as JsonArray
            // too (confidence 0.8). SmartCrusher's `crush` is safe to
            // call on those — it parses, finds no compressible
            // arrays, and returns the input.
            let result = smart_crusher().crush(text, EMPTY_QUERY, DEFAULT_BIAS);
            if !result.was_modified {
                return DispatchResult::NoOp {
                    content_type: content_type.as_str(),
                    declined_by: None,
                };
            }
            DispatchResult::Compressed {
                strategy: STRATEGY_SMART_CRUSHER,
                compressed: result.compressed,
            }
        }
        ContentType::BuildOutput => {
            let (result, _stats) = log_compressor().compress(text, DEFAULT_BIAS);
            if result.compressed == result.original {
                return DispatchResult::NoOp {
                    content_type: content_type.as_str(),
                    declined_by: None,
                };
            }
            DispatchResult::Compressed {
                strategy: STRATEGY_LOG_COMPRESSOR,
                compressed: result.compressed,
            }
        }
        ContentType::SearchResults => {
            let (result, _stats) = search_compressor().compress(text, EMPTY_QUERY, DEFAULT_BIAS);
            if result.compressed == result.original {
                return DispatchResult::NoOp {
                    content_type: content_type.as_str(),
                    declined_by: None,
                };
            }
            DispatchResult::Compressed {
                strategy: STRATEGY_SEARCH_COMPRESSOR,
                compressed: result.compressed,
            }
        }
        ContentType::GitDiff => {
            let result = diff_compressor().compress(text, EMPTY_QUERY);
            if result.compressed == text {
                return DispatchResult::NoOp {
                    content_type: content_type.as_str(),
                    declined_by: None,
                };
            }
            DispatchResult::Compressed {
                strategy: STRATEGY_DIFF_COMPRESSOR,
                compressed: result.compressed,
            }
        }
        ContentType::SourceCode => {
            // Off-arm (`--code-aware false`, via `set_code_aware_enabled`):
            // source passes through untouched, byte-identical.
            if !CODE_AWARE_ENABLED.load(Ordering::Relaxed) {
                return DispatchResult::NoOp {
                    content_type: content_type.as_str(),
                    declined_by: Some("code_aware_disabled"),
                };
            }
            let result = code_compressor().compress(text);
            // The engine returns the input unchanged for passthrough
            // branches (below min-tokens, UNKNOWN language, invalid-syntax
            // fallback, ratio guard). Treat that as NoOp.
            if result.compressed == text {
                return DispatchResult::NoOp {
                    content_type: content_type.as_str(),
                    declined_by: None,
                };
            }
            DispatchResult::Compressed {
                strategy: STRATEGY_CODE_COMPRESSOR,
                compressed: result.compressed,
            }
        }
        ContentType::StructuredConfig => {
            // Byte-reversible fold only: `compact_lossless` self-verifies the
            // round-trip and returns the input unchanged when it cannot, so a
            // config block is never mangled — at worst it passes through.
            let compressed =
                crate::transforms::lossless_compaction::compact_lossless(text, "config");
            if compressed.len() >= text.len() {
                return DispatchResult::NoOp {
                    content_type: content_type.as_str(),
                    declined_by: None,
                };
            }
            DispatchResult::Compressed {
                strategy: STRATEGY_CONFIG_LOSSLESS,
                compressed,
            }
        }
        ContentType::PlainText
            if crate::transforms::content_router::kompress_size_gate_exceeded(text) =>
        {
            // Size gate (#1171). ONNX inference is O(tokens) and runs
            // synchronously on the request thread; on a large or cold context
            // it blows the request budget and leaks a worker that cannot be
            // preempted. This is the ML boundary the live-zone path actually
            // uses, so the ceiling has to be enforced here — gating only
            // `content_router::try_kompress` would leave this path unprotected.
            crate::transforms::observability::observe_kompress_size_gate("exceeded");
            tracing::info!(
                approx_tokens = text.len() / 4,
                ceiling = crate::transforms::content_router::kompress_max_tokens(),
                "kompress size-gate fired; skipping ML for this block"
            );
            DispatchResult::NoOp {
                content_type: content_type.as_str(),
                declined_by: None,
            }
        }
        #[cfg(feature = "ml")]
        ContentType::PlainText => match kompress() {
            // Cache-only model present → let it score the prose. Passes
            // through (NoOp) when the model keeps everything or the input is
            // too short (engine returns the input unchanged).
            Some(model) => {
                crate::transforms::observability::observe_kompress_size_gate("within");
                let result = model.compress_with_ratio(text, config.target_ratio);
                if result.compressed == text {
                    return DispatchResult::NoOp {
                        content_type: content_type.as_str(),
                        declined_by: None,
                    };
                }
                DispatchResult::Compressed {
                    strategy: STRATEGY_KOMPRESS,
                    compressed: result.compressed,
                }
            }
            // Model not cached → passthrough, mirroring the Python reference's
            // "unavailable → unchanged" behavior. A background warm-up
            // (`warm_live_zone_compressors`) populates the cache off the
            // request path.
            None => DispatchResult::NoOp {
                content_type: content_type.as_str(),
                declined_by: None,
            },
        },
        // Same passthrough as an uncached model, reached without `ml` because
        // no prose compressor is built in at all.
        #[cfg(not(feature = "ml"))]
        ContentType::PlainText => DispatchResult::NoOp {
            content_type: content_type.as_str(),
            declined_by: None,
        },
        // No HTML compressor on the Rust side; pages are handled by
        // upstream extractors, not the proxy.
        ContentType::Html => DispatchResult::NoOp {
            content_type: content_type.as_str(),
            declined_by: None,
        },
        // Tabular data: treat like plain text — Kompress passthrough.
        ContentType::Tabular => DispatchResult::NoOp {
            content_type: content_type.as_str(),
            declined_by: None,
        },
    };

    // ─── "Did it actually help?" gate ──────────────────────────────────
    //
    // Every arm above decides on its own whether the compressor helped,
    // and most of them ask the wrong question: `compressed == original`.
    // Byte-identity only catches a compressor that returned its input
    // untouched. It misses the far more common failure — a compressor
    // that *rewrote* the block without removing anything from it. The
    // rewrite differs from the input, so it is tagged `Compressed`, and
    // the block then travels all the way to the tokenizer gate in
    // `compress_one_block` before being thrown away.
    //
    // SearchCompressor is the worst offender by construction: it parses
    // the grep lines, selects which matches to keep, and re-formats the
    // selection. When the caps never bite, the selection *is* the input,
    // so `format_output` re-emits every match — same content, normalized
    // separators, files in `BTreeMap` order. Different bytes, identical
    // (or larger) size.
    //
    // Measured on 862 captured production requests: of the search-
    // compressor blocks that the tokenizer gate rejected, 99.6% had
    // dropped *zero* matches, and 93% were already >= the input in
    // bytes. For the code compressor it was 100%. Meanwhile *no*
    // accepted compression in the whole corpus — 6116 search, 1216
    // code — grew in bytes. So bytes settle these cases on their own,
    // and settling them here is strictly cheaper than settling them
    // downstream: the caller skips two tokenizer passes over the block,
    // and, because a `NoOp` lands in the dispatch memo's *skip* tier
    // rather than as a cached `Compressed`, every later request
    // carrying the same block short-circuits instead of re-paying the
    // tokenizer. Conversation prefixes repeat constantly, so that
    // repeat cost is where the waste actually accumulated.
    //
    // This does not soften the tokenizer gate downstream. Bytes are a
    // one-way signal: not-smaller-in-bytes means there is nothing to
    // win, but smaller-in-bytes does not mean smaller in tokens. Any
    // candidate that does shrink in bytes still has to clear the
    // tokenizer before it reaches the wire — which is what that gate is
    // for, and it keeps catching the case it was built for (bytes down,
    // tokens up on dense or heavily-fragmented content).
    //
    // The `StructuredConfig` arm has always applied exactly this rule;
    // this just holds every compressor to it.
    if let DispatchResult::Compressed {
        compressed,
        strategy,
    } = &result
        && compressed.len() >= text.len()
    {
        // Carry which compressor declined. These blocks used to reach the
        // tokenizer and be counted as rejections; absorbing them here
        // without saying so would make this fix unfalsifiable — the
        // rejection counter would fall whether the waste went away or the
        // gate began declining work that pays.
        return DispatchResult::NoOp {
            content_type: content_type.as_str(),
            declined_by: Some(strategy),
        };
    }
    result
}

/// CTX-3: run the content-type detection + compressor stack over a
/// single block of text and return the structural digest.
///
/// This is the pure, provider-agnostic core the `ctx_offload` transform
/// reuses so it hits the *exact same* detectors and compressors as the
/// live-zone dispatcher (`dispatch_compressor`) rather than duplicating
/// that routing. It is a deterministic pure function of `text` (invariant
/// I1 in `docs/ctx-mode-in-headroom-plan.md`): `detect_content_type` is a
/// pure classifier and every compressor is a pure transform.
///
/// Returns `(Some(strategy), compressed)` when a compressor rewrote the
/// block, or `(None, text.to_owned())` when no compressor applied or the
/// content was left unchanged.
///
/// NOTE: the `PlainText` arm routes through `kompress`, whose output
/// depends on whether the local HF model is present in the on-disk cache.
/// That state is stable for a given deployment (present or absent per
/// machine) but is the one place offload determinism is environment- (not
/// byte-) scoped — same caveat the live-zone dispatcher already carries.
pub fn compress_block_for_offload(text: &str) -> (Option<&'static str>, String) {
    let detection = detect_content_type(text);
    match dispatch_compressor(text, detection.content_type) {
        DispatchResult::Compressed {
            strategy,
            compressed,
        } => (Some(strategy), compressed),
        _ => (None, text.to_string()),
    }
}

#[cfg(test)]
mod dispatch_cache_tests {
    use super::*;

    /// A log payload the log compressor reliably shrinks.
    fn sample_log() -> String {
        (0..40)
            .map(|i| format!("2026-07-27T10:00:{i:02}Z INFO worker handled request id={i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn compressed_of(result: &DispatchResult) -> Option<(&str, &str)> {
        match result {
            DispatchResult::Compressed {
                strategy,
                compressed,
            } => Some((strategy, compressed.as_str())),
            _ => None,
        }
    }

    /// The property that makes the memo safe to add to the hot path: a second
    /// call returns exactly what an uncached call would.
    #[test]
    fn a_cache_hit_matches_the_uncached_result() {
        let text = sample_log();
        let config = DispatchConfig::default();

        let uncached = dispatch_compressor_uncached(&text, ContentType::BuildOutput, &config);
        // First call populates, second call reads back through the memo.
        let _ = dispatch_compressor_with_config(&text, ContentType::BuildOutput, &config);
        let hit = dispatch_compressor_with_config(&text, ContentType::BuildOutput, &config);

        assert_eq!(compressed_of(&hit), compressed_of(&uncached));
    }

    /// `target_ratio` changes Kompress output, so it must not alias.
    #[test]
    fn the_key_separates_different_target_ratios() {
        let text = sample_log();
        let a = dispatch_cache_key(&text, ContentType::BuildOutput, Some(0.3));
        let b = dispatch_cache_key(&text, ContentType::BuildOutput, Some(0.7));
        let none = dispatch_cache_key(&text, ContentType::BuildOutput, None);

        assert_ne!(a, b);
        assert_ne!(a, none);
        assert_ne!(b, none);
    }

    /// The same bytes routed as a different content type pick a different
    /// compressor, so those must not share an entry either.
    #[test]
    fn the_key_separates_different_content_types() {
        let text = sample_log();
        assert_ne!(
            dispatch_cache_key(&text, ContentType::BuildOutput, None),
            dispatch_cache_key(&text, ContentType::PlainText, None),
        );
    }

    #[test]
    fn the_key_separates_different_content() {
        assert_ne!(
            dispatch_cache_key("alpha", ContentType::BuildOutput, None),
            dispatch_cache_key("bravo", ContentType::BuildOutput, None),
        );
    }

    /// Only strategies this module actually emits may be revived from a cache
    /// entry; anything else must force a recompress rather than be leaked.
    #[test]
    fn only_known_strategies_are_interned() {
        for known in [
            STRATEGY_SMART_CRUSHER,
            STRATEGY_LOG_COMPRESSOR,
            STRATEGY_SEARCH_COMPRESSOR,
            STRATEGY_DIFF_COMPRESSOR,
            STRATEGY_CODE_COMPRESSOR,
            STRATEGY_CONFIG_LOSSLESS,
            STRATEGY_KOMPRESS,
        ] {
            assert_eq!(intern_dispatch_strategy(known), Some(known));
        }
        assert_eq!(
            intern_dispatch_strategy("strategy_from_a_future_version"),
            None
        );
        assert_eq!(intern_dispatch_strategy(""), None);
    }

    /// Empty input short-circuits before the cache is touched.
    #[test]
    fn empty_content_is_a_noop_without_caching() {
        let result = dispatch_compressor_with_config(
            "",
            ContentType::BuildOutput,
            &DispatchConfig::default(),
        );
        assert!(matches!(result, DispatchResult::NoOp { .. }));
    }

    /// A declined block is remembered as a skip, and replaying it still reports
    /// NoOp rather than resurrecting a bogus compressed result.
    #[test]
    fn a_declined_block_stays_declined() {
        // Html has no compressor, so dispatch always declines it.
        let text = "<p>hello</p>";
        let config = DispatchConfig::default();

        let first = dispatch_compressor_with_config(text, ContentType::Html, &config);
        let second = dispatch_compressor_with_config(text, ContentType::Html, &config);

        assert!(matches!(first, DispatchResult::NoOp { .. }));
        assert!(matches!(second, DispatchResult::NoOp { .. }));
    }
}

#[cfg(test)]
mod dispatch_cache_kompress_flag_tests {
    use super::*;

    /// Regression: `set_kompress_enabled` is a runtime toggle that changes what
    /// dispatch produces for `PlainText`. It is not a function argument, so it
    /// has to be folded into the key explicitly — otherwise a result computed
    /// while Kompress was off would be replayed after it was switched on.
    #[test]
    fn the_key_separates_the_kompress_enabled_flag() {
        let text = "some plain prose that kompress would consider compressing";
        let before = set_kompress_enabled_for_test(false);
        let key_disabled = dispatch_cache_key(text, ContentType::PlainText, None);
        set_kompress_enabled_for_test(true);
        let key_enabled = dispatch_cache_key(text, ContentType::PlainText, None);
        set_kompress_enabled_for_test(before);

        assert_ne!(key_disabled, key_enabled);
    }

    /// Sets the flag and returns its previous value, so a test can restore it.
    fn set_kompress_enabled_for_test(enabled: bool) -> bool {
        KOMPRESS_ENABLED.swap(enabled, std::sync::atomic::Ordering::Relaxed)
    }
}
