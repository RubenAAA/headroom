//! Cache-key contract: byte-stability pins for every prefix-cache key builder.
//!
//! Small edits in these modules have outsized blast radius (a changed key =
//! a full recache for every session). These tests fail on any silent change
//! to derivation, placement, ordering, or identity inputs. Unit tests cover
//! branches; this file pins the cross-module contract in one place.
//!
//! Run: `cargo nextest run -p headroom-proxy --test cache`
//! Mapped by `scripts/what-to-run.sh` to any `cache_stabilization/*` change.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::http::HeaderMap;
use serde_json::json;

use headroom_proxy::cache_stabilization::anthropic_cache_control::{
    AutoPlaceOutcome, auto_place_anthropic_cache_control,
};
use headroom_proxy::cache_stabilization::drift_detector::{ApiKind, derive_session_key_with_model};
use headroom_proxy::cache_stabilization::message_breakpoints::{
    push_marker_to_tail, push_newest_marker_to_tail,
};
use headroom_proxy::cache_stabilization::openai_cache_key::{
    KEY_HEX_LEN, OpenAiShape, inject_prompt_cache_key,
};
use headroom_proxy::cache_stabilization::tool_roster_pin::RosterPinStore;

fn addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 1234)
}

// ── E4: OpenAI prompt_cache_key ──────────────────────────────────────────

#[test]
fn e4_key_is_deterministic_for_fixed_fixture() {
    let template = || {
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "contract fixture system"},
                {"role": "user", "content": "turn one"}
            ],
            "tools": [{"type": "function", "function": {"name": "search"}}]
        })
    };
    let mut a = template();
    let mut b = template();
    inject_prompt_cache_key(&mut a, OpenAiShape::ChatCompletions);
    inject_prompt_cache_key(&mut b, OpenAiShape::ChatCompletions);
    assert_eq!(a["prompt_cache_key"], b["prompt_cache_key"]);
    let key = a["prompt_cache_key"].as_str().unwrap().to_string();
    assert_eq!(key.len(), KEY_HEX_LEN);
    assert!(key.bytes().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn e4_key_ignores_user_turns_but_not_model_system_tools() {
    let base = || {
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "contract fixture system"},
                {"role": "user", "content": "PLACEHOLDER"}
            ],
            "tools": [{"type": "function", "function": {"name": "search"}}]
        })
    };
    let mut a = base();
    a["messages"][1]["content"] = json!("first question");
    let mut b = base();
    b["messages"][1]["content"] = json!("a totally different second question with more text");
    inject_prompt_cache_key(&mut a, OpenAiShape::ChatCompletions);
    inject_prompt_cache_key(&mut b, OpenAiShape::ChatCompletions);
    assert_eq!(
        a["prompt_cache_key"], b["prompt_cache_key"],
        "user-turn variation must not rotate the cache key"
    );

    for (mut body, label) in [(base(), "model"), (base(), "system"), (base(), "tools")] {
        match label {
            "model" => body["model"] = json!("gpt-4o-mini"),
            "system" => body["messages"][0]["content"] = json!("different system"),
            "tools" => {
                body["tools"] = json!([{"type": "function", "function": {"name": "lookup"}}])
            }
            _ => unreachable!(),
        }
        let mut reference = base();
        inject_prompt_cache_key(&mut reference, OpenAiShape::ChatCompletions);
        inject_prompt_cache_key(&mut body, OpenAiShape::ChatCompletions);
        assert_ne!(
            reference["prompt_cache_key"], body["prompt_cache_key"],
            "{label} change must rotate the cache key"
        );
    }
}

#[test]
fn e4_golden_vector() {
    // Golden pin: any change to model/system/tools hashing rotates this.
    let mut body = json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": "contract fixture system"},
            {"role": "user", "content": "turn one"}
        ],
        "tools": [{"type": "function", "function": {"name": "search"}}]
    });
    inject_prompt_cache_key(&mut body, OpenAiShape::ChatCompletions);
    let key = body["prompt_cache_key"].as_str().unwrap();
    assert_eq!(key.len(), KEY_HEX_LEN, "golden key must stay 32 hex chars");
    // Pinned on first green run; update deliberately, never silently.
    assert_eq!(key, "e975e776bf4dea6d359f639d368f5875");
}

// ── E3: Anthropic cache_control auto-placement ───────────────────────────

#[test]
fn e3_places_exactly_one_ephemeral_marker_on_last_tool() {
    let mut body = json!({
        "model": "claude-sonnet-4-6",
        "system": "contract system",
        "tools": [
            {"name": "a", "description": "a"},
            {"name": "b", "description": "b"}
        ],
        "messages": [{"role": "user", "content": "hi"}],
    });
    let outcome = auto_place_anthropic_cache_control(&mut body);
    assert_eq!(
        outcome,
        AutoPlaceOutcome::Applied {
            placed_count: 1,
            locations: vec!["tools[1]".to_string()],
        }
    );
    assert_eq!(
        body.pointer("/tools/1/cache_control"),
        Some(&json!({"type": "ephemeral"})),
        "marker shape is part of the contract"
    );
    assert!(body.pointer("/tools/0/cache_control").is_none());
    // Second run is a no-op via customer-placement-wins.
    let before = body.clone();
    let second = auto_place_anthropic_cache_control(&mut body);
    assert!(matches!(second, AutoPlaceOutcome::Skipped { .. }));
    assert_eq!(body, before);
}

// ── Tail breakpoints ─────────────────────────────────────────────────────

#[test]
fn tail_breakpoint_moves_newest_marker_to_last_block_only() {
    let mut body = json!({"messages": [
        {"role": "user", "content": [{"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}]},
        {"role": "user", "content": [{"type": "text", "text": "b"}]},
        {"role": "user", "content": [{"type": "text", "text": "c"}]},
    ]});
    assert!(push_marker_to_tail(&mut body));
    let msgs = body["messages"].as_array().unwrap();
    assert!(msgs[0]["content"][0].get("cache_control").is_none());
    assert!(msgs[2]["content"][0].get("cache_control").is_some());
    // Already at tail: no-op, byte-identical.
    let before = body.clone();
    assert!(!push_marker_to_tail(&mut body));
    assert_eq!(body, before);
    // Continuation path moves only the newest of two.
    let mut two = json!({"messages": [
        {"role": "user", "content": [{"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}]},
        {"role": "user", "content": [{"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}}]},
        {"role": "user", "content": [{"type": "text", "text": "c"}]},
    ]});
    assert!(
        !push_marker_to_tail(&mut two),
        "client path leaves two alone"
    );
    assert!(push_newest_marker_to_tail(&mut two));
    let msgs = two["messages"].as_array().unwrap();
    assert!(
        msgs[0]["content"][0].get("cache_control").is_some(),
        "older marker stays"
    );
    assert!(
        msgs[2]["content"][0].get("cache_control").is_some(),
        "newest follows tail"
    );
}

// ── B3: roster pin ordering ──────────────────────────────────────────────

#[test]
fn roster_pin_reinserts_at_position_and_appends_new_at_tail() {
    let store = RosterPinStore::default();
    let t = |n: &str| json!({"name": n, "input_schema": {"type": "object"}});
    let mut first = vec![t("a"), t("send_user_file"), t("c")];
    store.pin("contract-session", "opus", &mut first);

    let mut second = vec![t("a"), t("c")];
    let out = store.pin("contract-session", "opus", &mut second);
    assert_eq!(out.reinserted, vec!["send_user_file".to_string()]);
    let names: Vec<_> = second.iter().map(|v| v["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["a", "send_user_file", "c"],
        "byte position is the contract"
    );

    // Different model must not inherit the roster.
    let mut other = vec![t("c")];
    let out = store.pin("contract-session", "sonnet", &mut other);
    assert!(!out.changed());
}

// ── E6: session identity ─────────────────────────────────────────────────

#[test]
fn session_key_is_stable_per_model_and_separates_models() {
    let headers = HeaderMap::new();
    let body = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "contract opener"}],
    });
    let k1 =
        derive_session_key_with_model(&headers, &addr(), &body, ApiKind::Anthropic, Some("opus"));
    let k2 =
        derive_session_key_with_model(&headers, &addr(), &body, ApiKind::Anthropic, Some("opus"));
    assert_eq!(k1, k2, "same conversation + same identity model = same key");
    let k3 =
        derive_session_key_with_model(&headers, &addr(), &body, ApiKind::Anthropic, Some("sonnet"));
    assert_ne!(
        k1, k3,
        "identity model is part of the key; reroutes must pass it through"
    );
}
