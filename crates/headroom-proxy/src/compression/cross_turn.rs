//! Cross-turn (whole-conversation) verbatim de-dup — request-level
//! post-pass.
//!
//! Port of the Python router hook
//! `ContentRouter._cross_turn_dedup_messages` (gated there by
//! `ContentRouterConfig.enable_cross_turn_dedup = False`). The span-level
//! algorithm lives in
//! [`headroom_core::transforms::cross_turn_dedup`]; this module is the
//! proxy-side orchestration: it runs AFTER the per-block live-zone
//! dispatcher, over the FINAL block forms, and rewrites later verbatim
//! tool-output spans into compact in-context pointers to the earlier
//! occurrence.
//!
//! # Cache-safety
//!
//! The core pass is prefix-monotonic: a block is only ever matched
//! against strictly earlier blocks, and the earliest occurrence is never
//! rewritten. Appending a turn therefore never mutates an earlier turn's
//! emitted bytes — the upstream prompt-cache prefix stays byte-stable.
//! Frozen-prefix messages (below the `cache_control`-derived floor) and
//! blocks carrying a `cache_control` key are reference targets only.
//!
//! # Failure-mode contract
//!
//! Same as the rest of the compression stack: this pass must NEVER break
//! a request. Any parse/serialize hiccup falls through to the incoming
//! [`Outcome`] unchanged.

use bytes::Bytes;
use headroom_core::transforms::cross_turn_dedup::dedup_messages;
use serde_json::Value;

use crate::compression::{resolve_frozen_count, Outcome};
use crate::config::{CompressionMode, Config};

/// Strategy tag surfaced on [`Outcome::Compressed::strategies_applied`]
/// when at least one span folded (Python appends
/// `router:cross_turn_dedup:{n}` to `transforms_applied`; the proxy's
/// strategy vocabulary is bare tags, counts go to the structured log).
pub const CROSS_TURN_DEDUP_STRATEGY: &str = "cross_turn_dedup";

/// Apply the cross-turn dedup post-pass to a dispatcher [`Outcome`].
///
/// - Flag off (`config.enable_cross_turn_dedup == false`, the default) or
///   compression mode `Off`: returns `outcome` unchanged.
/// - [`Outcome::Passthrough`]: body shape already ruled out — unchanged.
/// - [`Outcome::NoCompression`] / [`Outcome::Compressed`]: parses the
///   would-be-forwarded body, runs
///   [`headroom_core::transforms::cross_turn_dedup::dedup_messages`] over
///   `messages` with the `cache_control`-derived frozen floor, and — when
///   at least one span folded — returns [`Outcome::Compressed`] carrying
///   the rewritten body with the `cross_turn_dedup` strategy tag
///   appended.
pub fn apply_cross_turn_dedup(
    outcome: Outcome,
    original_body: &Bytes,
    config: &Config,
    path: &'static str,
    request_id: &str,
) -> Outcome {
    if !config.enable_cross_turn_dedup || matches!(config.compression_mode, CompressionMode::Off) {
        return outcome;
    }

    // The body the proxy would forward if this pass did nothing.
    let mut parsed: Value = {
        let input: &[u8] = match &outcome {
            Outcome::Passthrough { .. } => return outcome,
            Outcome::Compressed { body, .. } => body.as_ref(),
            Outcome::NoCompression => original_body.as_ref(),
        };
        match serde_json::from_slice(input) {
            Ok(v) => v,
            // Never break the request: forward whatever we had.
            Err(_) => return outcome,
        }
    };

    let frozen_count = resolve_frozen_count(&parsed, config.cache_control_auto_frozen, request_id);

    let Some(messages) = parsed.get_mut("messages").and_then(Value::as_array_mut) else {
        // No `messages` array (e.g. /v1/responses `input` shape) —
        // nothing this pass knows how to dedup.
        return outcome;
    };

    let stats = dedup_messages(messages, frozen_count);
    if stats.spans_folded == 0 {
        return outcome;
    }

    let new_body = match serde_json::to_vec(&parsed) {
        Ok(v) => Bytes::from(v),
        Err(err) => {
            // We just parsed this value; serialize-back failure is
            // unreachable in practice. Loud log, forward untouched.
            tracing::error!(
                event = "cross_turn_dedup_serialize_failed",
                request_id = %request_id,
                path = path,
                error = %err,
                "cross-turn dedup folded spans but serialize-back failed; \
                 forwarding pre-dedup bytes"
            );
            return outcome;
        }
    };

    tracing::info!(
        event = "cross_turn_dedup_applied",
        request_id = %request_id,
        path = path,
        spans_folded = stats.spans_folded,
        lines_removed = stats.lines_removed,
        chars_removed = stats.chars_removed,
        blocks_scanned = stats.blocks,
        frozen_message_count = frozen_count,
        body_bytes_out = new_body.len(),
        "cross-turn dedup folded verbatim re-read span(s) into in-context pointer(s)"
    );

    match outcome {
        Outcome::Compressed {
            body: _,
            tokens_before,
            tokens_after,
            mut strategies_applied,
            markers_inserted,
            per_strategy_tokens,
        } => {
            if !strategies_applied.contains(&CROSS_TURN_DEDUP_STRATEGY) {
                strategies_applied.push(CROSS_TURN_DEDUP_STRATEGY);
            }
            Outcome::Compressed {
                body: new_body,
                tokens_before,
                tokens_after,
                strategies_applied,
                markers_inserted,
                per_strategy_tokens,
            }
        }
        Outcome::NoCompression => Outcome::Compressed {
            body: new_body,
            // Like the Phase E normalization surface: no per-strategy
            // token accounting here — emit-site falls back to one
            // aggregate sample.
            tokens_before: 0,
            tokens_after: 0,
            strategies_applied: vec![CROSS_TURN_DEDUP_STRATEGY],
            markers_inserted: Vec::new(),
            per_strategy_tokens: Vec::new(),
        },
        // Filtered out above before the parse.
        Outcome::Passthrough { .. } => unreachable!("passthrough returned early"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CompressionMode;
    use url::Url;

    fn test_config(enabled: bool) -> Config {
        let mut cfg = Config::for_test(Url::parse("http://127.0.0.1:9").unwrap());
        cfg.enable_cross_turn_dedup = enabled;
        cfg.compression_mode = CompressionMode::LiveZone;
        cfg
    }

    fn span() -> String {
        (0..12)
            .map(|i| {
                format!(
                    "    result_{i} = compute_overdraft(business_id={i}, amount={})",
                    i * 100
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn body_with_reread(span: &str) -> Bytes {
        let body = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "messages": [
                {"role": "user", "content": "fix the overdraft bug"},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1",
                     "content": format!("$ cat merge.py\n{span}\n# end")},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t2",
                     "content": format!("$ sed -n 1,20p merge.py\n{span}\n# more")},
                ]},
            ],
        });
        Bytes::from(serde_json::to_vec(&body).unwrap())
    }

    fn tool_texts(body: &[u8]) -> Vec<String> {
        let parsed: Value = serde_json::from_slice(body).unwrap();
        parsed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            .map(|b| b["content"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn flag_off_is_a_noop() {
        let body = body_with_reread(&span());
        let out = apply_cross_turn_dedup(
            Outcome::NoCompression,
            &body,
            &test_config(false),
            "/v1/messages",
            "req-ct-1",
        );
        assert!(matches!(out, Outcome::NoCompression));
    }

    #[test]
    fn mode_off_is_a_noop_even_with_flag_on() {
        let body = body_with_reread(&span());
        let mut cfg = test_config(true);
        cfg.compression_mode = CompressionMode::Off;
        let out = apply_cross_turn_dedup(
            Outcome::NoCompression,
            &body,
            &cfg,
            "/v1/messages",
            "req-ct-2",
        );
        assert!(matches!(out, Outcome::NoCompression));
    }

    #[test]
    fn reread_is_folded_and_earliest_untouched() {
        let s = span();
        let body = body_with_reread(&s);
        let out = apply_cross_turn_dedup(
            Outcome::NoCompression,
            &body,
            &test_config(true),
            "/v1/messages",
            "req-ct-3",
        );
        let Outcome::Compressed {
            body: new_body,
            strategies_applied,
            ..
        } = out
        else {
            panic!("expected Compressed outcome");
        };
        assert!(strategies_applied.contains(&CROSS_TURN_DEDUP_STRATEGY));
        let texts = tool_texts(&new_body);
        assert!(
            texts[0].contains(&s),
            "earliest occurrence must stay verbatim"
        );
        assert!(!texts[0].contains("same as msg "));
        assert!(texts[1].contains("same as msg "), "later re-read must fold");
        assert!(new_body.len() < body.len());
    }

    #[test]
    fn cache_control_block_is_reference_target_only() {
        let s = span();
        let body = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": s,
                     "cache_control": {"type": "ephemeral"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t2",
                     "content": format!("later:\n{s}")},
                ]},
            ],
        });
        let body = Bytes::from(serde_json::to_vec(&body).unwrap());
        let out = apply_cross_turn_dedup(
            Outcome::NoCompression,
            &body,
            &test_config(true),
            "/v1/messages",
            "req-ct-4",
        );
        let Outcome::Compressed { body: new_body, .. } = out else {
            panic!("expected Compressed outcome");
        };
        let texts = tool_texts(&new_body);
        assert_eq!(texts[0], s, "cache_control block must never be rewritten");
        assert!(texts[1].contains("same as msg "));
    }

    #[test]
    fn no_duplicate_no_change() {
        let body = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": span()},
                ]},
            ],
        });
        let body = Bytes::from(serde_json::to_vec(&body).unwrap());
        let out = apply_cross_turn_dedup(
            Outcome::NoCompression,
            &body,
            &test_config(true),
            "/v1/messages",
            "req-ct-5",
        );
        assert!(matches!(out, Outcome::NoCompression));
    }

    #[test]
    fn responses_shaped_body_without_messages_is_a_noop() {
        let body = serde_json::json!({
            "model": "gpt-4o",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
        });
        let body = Bytes::from(serde_json::to_vec(&body).unwrap());
        let out = apply_cross_turn_dedup(
            Outcome::NoCompression,
            &body,
            &test_config(true),
            "/v1/responses",
            "req-ct-6",
        );
        assert!(matches!(out, Outcome::NoCompression));
    }

    #[test]
    fn compressed_outcome_keeps_existing_strategies_and_folds() {
        let s = span();
        let body = body_with_reread(&s);
        let prior = Outcome::Compressed {
            body: body.clone(),
            tokens_before: 100,
            tokens_after: 80,
            strategies_applied: vec!["smart_crusher"],
            markers_inserted: vec!["m1".to_string()],
            per_strategy_tokens: Vec::new(),
        };
        let out =
            apply_cross_turn_dedup(prior, &body, &test_config(true), "/v1/messages", "req-ct-7");
        let Outcome::Compressed {
            strategies_applied,
            tokens_before,
            tokens_after,
            markers_inserted,
            body: new_body,
            ..
        } = out
        else {
            panic!("expected Compressed outcome");
        };
        assert_eq!(
            strategies_applied,
            vec!["smart_crusher", CROSS_TURN_DEDUP_STRATEGY]
        );
        assert_eq!((tokens_before, tokens_after), (100, 80));
        assert_eq!(markers_inserted, vec!["m1".to_string()]);
        assert!(tool_texts(&new_body)[1].contains("same as msg "));
    }

    #[test]
    fn openai_tool_role_string_content_folds() {
        let s = span();
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "tool", "tool_call_id": "t1", "content": format!("a\n{s}")},
                {"role": "tool", "tool_call_id": "t2", "content": format!("b\n{s}")},
            ],
        });
        let body = Bytes::from(serde_json::to_vec(&body).unwrap());
        let out = apply_cross_turn_dedup(
            Outcome::NoCompression,
            &body,
            &test_config(true),
            "/v1/chat/completions",
            "req-ct-8",
        );
        let Outcome::Compressed { body: new_body, .. } = out else {
            panic!("expected Compressed outcome");
        };
        let parsed: Value = serde_json::from_slice(&new_body).unwrap();
        let msgs = parsed["messages"].as_array().unwrap();
        assert!(!msgs[0]["content"]
            .as_str()
            .unwrap()
            .contains("same as msg "));
        assert!(msgs[1]["content"]
            .as_str()
            .unwrap()
            .contains("same as msg "));
    }

    #[test]
    fn prefix_stability_across_appended_turn() {
        // Router-level cache-safety (mirrors Python's
        // test_apply_dedups_reread_and_keeps_prefix_stable): running the
        // pass on turn k and on turn k+1 leaves the first k messages'
        // emitted bytes identical.
        let s = span();
        let msgs_short = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "messages": [
                {"role": "user", "content": "fix the overdraft bug"},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1",
                     "content": format!("$ cat merge.py\n{s}\n# end")},
                ]},
            ],
        });
        let body_short = Bytes::from(serde_json::to_vec(&msgs_short).unwrap());
        let body_long = body_with_reread(&s);

        let cfg = test_config(true);
        let out_short = apply_cross_turn_dedup(
            Outcome::NoCompression,
            &body_short,
            &cfg,
            "/v1/messages",
            "req-ct-9a",
        );
        // Short conversation: nothing to fold, bytes unchanged.
        assert!(matches!(out_short, Outcome::NoCompression));

        let out_long = apply_cross_turn_dedup(
            Outcome::NoCompression,
            &body_long,
            &cfg,
            "/v1/messages",
            "req-ct-9b",
        );
        let Outcome::Compressed { body: new_body, .. } = out_long else {
            panic!("expected Compressed outcome");
        };
        // The earlier turn's tool_result bytes are identical to the
        // short-conversation form — appending a turn changed nothing
        // upstream of it.
        assert_eq!(tool_texts(&new_body)[0], tool_texts(&body_short)[0]);
    }
}
