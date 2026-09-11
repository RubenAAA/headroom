//! P3 acceptance — cross-session gate seeding through the public API.
//!
//! Drives the real components in the order the Claude/routed wiring blocks
//! do: derive session → drift birth check → seed-on-birth → offload with
//! `rebuild_boundary = dims.is_some()`. No mocks: real `DriftState`, real
//! `OffloadGate`, real `offload_anthropic_request`.
//!
//! Lanes are intentionally NOT used here. Both wirings pass *session* keys
//! to the gate and the helper (the gate is session-keyed; lanes only scope
//! drift baselines), so session-keyed drift is the faithful harness. What
//! this pins beyond the P1/P2 unit tests is the *composition*: birth
//! detection feeding the helper feeding the offload, across model switches,
//! credentials, and process restarts.

use headroom_proxy::cache_stabilization::drift_detector::{
    compute_structural_hash, derive_session_key, observe_drift_with_birth, ApiKind, DriftState,
};
use headroom_proxy::compression::ctx_offload::{
    offload_anthropic_request, seed_newborn_session, CtxOffloadConfig, OffloadGate, OffloadPolicy,
};
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tempfile::TempDir;

const MIN_BYTES: usize = 2_000;

fn gate_cfg() -> CtxOffloadConfig {
    CtxOffloadConfig {
        min_bytes: MIN_BYTES,
        stale_margin: 0,
        stale_window: 0,
        // Mechanism under test; the flag plumbing itself is P2-tested.
        cross_session_seed: true,
    }
}

fn headers_for(token: &str) -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(axum::http::header::AUTHORIZATION, token.parse().unwrap());
    headers
}

fn addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242)
}

/// A large tool_result body (> MIN_BYTES).
fn big_log() -> String {
    "[x] ERROR: disk full while writing chunk\nretrying...\n".repeat(80)
}

/// Client history: stable opener, one assistant tool_use, one big
/// tool_result, then `tail_pairs` small turns so the result sits frozen
/// (defers without a boundary — the stall seeding removes).
fn history(extra_pairs: usize) -> Vec<Value> {
    let mut messages = vec![
        json!({"role":"user","content":"shared opener"}),
        json!({"role":"assistant","content":[
            {"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"cat big.log"}}
        ]}),
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"tu_1","content": big_log()}
        ]}),
    ];
    for i in 0..extra_pairs {
        messages.push(
            json!({"role":"assistant","content":[{"type":"text","text":format!("noted {i}")}]}),
        );
        messages
            .push(json!({"role":"user","content":[{"type":"text","text":format!("next {i}")}]}));
    }
    messages
}

fn result_text(body: &Value) -> String {
    body["messages"][2]["content"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

struct Turn {
    session: String,
    birth: bool,
    body: Value,
    offloaded: usize,
    deferred: usize,
}

/// One request through the wired order: derive → drift → seed-on-birth →
/// offload with `rebuild_boundary = dims.is_some()`. Mirrors both call
/// sites; `seed_on_birth` stands in for the flag + `lane_birth` conditional.
#[allow(clippy::too_many_arguments)]
fn run_turn(
    drift: &DriftState,
    gate: &OffloadGate,
    headers: &axum::http::HeaderMap,
    addr: &SocketAddr,
    raw: &[Value],
    model: &str,
    system: &str,
    seed_on_birth: bool,
) -> Turn {
    let mut body = json!({"model": model, "system": system, "messages": raw});
    let session = derive_session_key(headers, addr, &body, ApiKind::Anthropic);
    let hash = compute_structural_hash(&body, ApiKind::Anthropic);
    let (dims, birth) = observe_drift_with_birth(drift, &session, hash);
    if birth && seed_on_birth {
        seed_newborn_session(
            gate,
            headers,
            addr,
            &body,
            ApiKind::Anthropic,
            &session,
            "req-test",
        );
    }
    let policy = OffloadPolicy {
        gate,
        session_key: &session,
        rebuild_boundary: dims.is_some(),
    };
    let out = offload_anthropic_request(&mut body, &gate_cfg(), Some(&policy));
    Turn {
        session,
        birth,
        body,
        offloaded: out.blocks_offloaded,
        deferred: out.blocks_deferred,
    }
}

#[test]
fn model_switch_converts_donor_known_block_on_first_sight() {
    // The reported symptom, end to end: opus warms a session, the same
    // conversation continues on sonnet, and sonnet's frozen history converts
    // immediately instead of stalling Deferred.
    let drift = DriftState::new(64);
    let gate = OffloadGate::new(64);
    let headers = headers_for("Bearer shared-workspace-token");
    let addr = addr();
    let raw = history(1);

    // Opus turn 1: birth, no boundary → the stall is real.
    let t1 = run_turn(
        &drift,
        &gate,
        &headers,
        &addr,
        &raw,
        "opus-large",
        "sys",
        true,
    );
    assert!(t1.birth);
    assert_eq!((t1.offloaded, t1.deferred), (0, 1));

    // Opus turn 2: a system tweak opens a boundary → converts.
    let t2 = run_turn(
        &drift,
        &gate,
        &headers,
        &addr,
        &raw,
        "opus-large",
        "sys v2",
        true,
    );
    assert!(!t2.birth);
    assert_eq!(t2.offloaded, 1);
    let donor_digest = result_text(&t2.body);
    assert!(donor_digest.contains("offloaded"), "digest, not raw");

    // Sonnet turn 1: same history, new session. Birth + seeding converts on
    // sight with the donor's exact bytes — no boundary needed.
    let s1 = run_turn(
        &drift,
        &gate,
        &headers,
        &addr,
        &raw,
        "sonnet-small",
        "sys v2",
        true,
    );
    assert!(s1.birth);
    assert_ne!(s1.session, t2.session, "model switch mints a new session");
    assert_eq!((s1.offloaded, s1.deferred), (1, 0));
    assert_eq!(result_text(&s1.body), donor_digest);
}

#[test]
fn wiring_without_birth_gate_leaves_live_sessions_converting_nothing() {
    // Pins the caller side of the S1a contract at flow level: the helper
    // runs only on birth. A live session's steady turns defer, and the gate
    // learns nothing about it.
    let drift = DriftState::new(64);
    let gate = OffloadGate::new(64);
    let headers = headers_for("Bearer tok");
    let addr = addr();
    let raw = history(1);

    let t1 = run_turn(
        &drift,
        &gate,
        &headers,
        &addr,
        &raw,
        "opus-large",
        "sys",
        true,
    );
    assert!(t1.birth && t1.offloaded == 0);
    // Same body, second turn: stable, no birth → helper skipped → defers.
    let t2 = run_turn(
        &drift,
        &gate,
        &headers,
        &addr,
        &raw,
        "opus-large",
        "sys",
        true,
    );
    assert!(!t2.birth);
    assert_eq!((t2.offloaded, t2.deferred), (0, 1));
}

#[test]
fn different_credential_does_not_seed() {
    // Same bytes, different bearer: lineage differs, so nothing seeds and
    // the newborn session defers exactly as it does today.
    let drift = DriftState::new(64);
    let gate = OffloadGate::new(64);
    let addr = addr();
    let raw = history(1);

    let donor_headers = headers_for("Bearer cred-A");
    let d1 = run_turn(
        &drift,
        &gate,
        &donor_headers,
        &addr,
        &raw,
        "opus-large",
        "sys",
        true,
    );
    assert!(d1.birth);
    let d2 = run_turn(
        &drift,
        &gate,
        &donor_headers,
        &addr,
        &raw,
        "opus-large",
        "sys v2",
        true,
    );
    assert_eq!(d2.offloaded, 1, "donor converts on its boundary");

    let other_headers = headers_for("Bearer cred-B");
    let n1 = run_turn(
        &drift,
        &gate,
        &other_headers,
        &addr,
        &raw,
        "sonnet-small",
        "sys v2",
        true,
    );
    assert!(n1.birth);
    assert_ne!(n1.session, d2.session);
    assert_eq!(
        (n1.offloaded, n1.deferred),
        (0, 1),
        "no donor across credentials: defers, converts nothing"
    );
}

#[test]
fn same_session_shares_gate_with_no_seeding() {
    // Intra-session path needs nothing new: a boundary conversion followed
    // by a steady turn re-applies through `prior` with no helper involved.
    let drift = DriftState::new(64);
    let gate = OffloadGate::new(64);
    let headers = headers_for("Bearer tok");
    let addr = addr();
    let raw = history(1);

    let t1 = run_turn(
        &drift,
        &gate,
        &headers,
        &addr,
        &raw,
        "opus-large",
        "sys",
        false,
    );
    assert_eq!((t1.offloaded, t1.deferred), (0, 1));
    let t2 = run_turn(
        &drift,
        &gate,
        &headers,
        &addr,
        &raw,
        "opus-large",
        "sys v2",
        false,
    );
    assert_eq!(t2.offloaded, 1);
    let digest = result_text(&t2.body);
    // Steady turn, helper never called (seed_on_birth=false throughout):
    // still converted, same bytes.
    let t3 = run_turn(
        &drift,
        &gate,
        &headers,
        &addr,
        &raw,
        "opus-large",
        "sys v2",
        false,
    );
    assert_eq!(t3.offloaded, 1);
    assert_eq!(result_text(&t3.body), digest);
}

#[test]
fn seeded_set_survives_gate_persistence_round_trip() {
    // Donor converts, process "restarts" (gate dropped, same dir), newborn
    // seeds from the persisted file via the hydrate fallback.
    let dir = TempDir::new().unwrap();
    let headers = headers_for("Bearer tok");
    let addr = addr();
    let raw = history(1);

    let donor_digest = {
        let drift = DriftState::new(64);
        let gate = OffloadGate::with_persistence(64, dir.path().to_path_buf());
        let t1 = run_turn(
            &drift,
            &gate,
            &headers,
            &addr,
            &raw,
            "opus-large",
            "sys",
            true,
        );
        assert_eq!((t1.offloaded, t1.deferred), (0, 1));
        let t2 = run_turn(
            &drift,
            &gate,
            &headers,
            &addr,
            &raw,
            "opus-large",
            "sys v2",
            true,
        );
        assert_eq!(t2.offloaded, 1);
        result_text(&t2.body)
    };

    let drift = DriftState::new(64);
    let gate = OffloadGate::with_persistence(64, dir.path().to_path_buf());
    let s1 = run_turn(
        &drift,
        &gate,
        &headers,
        &addr,
        &raw,
        "sonnet-small",
        "sys v2",
        true,
    );
    assert!(s1.birth);
    assert_eq!(
        (s1.offloaded, s1.deferred),
        (1, 0),
        "seeds from disk after restart"
    );
    assert_eq!(result_text(&s1.body), donor_digest);
}

#[test]
fn upstream_prefix_stays_byte_stable_across_model_switch() {
    // I5-style golden: the client resends full raw history every turn; what
    // the provider caches must then only ever grow — except the single
    // priced rewrite where a raw block first converts (the boundary). After
    // that conversion, every turn re-emits it byte-identically, across a
    // model switch too. Top-level `model`/`system` may differ; the messages
    // array must not, so everything below serializes messages to bytes the
    // way the provider's cache key sees them.
    let drift = DriftState::new(64);
    let gate = OffloadGate::new(64);
    let headers = headers_for("Bearer tok");
    let addr = addr();

    fn ser(messages: &[Value]) -> Vec<Vec<u8>> {
        messages
            .iter()
            .map(|m| serde_json::to_vec(m).unwrap())
            .collect()
    }

    let mut raw = history(1);
    let raw_base = ser(&raw);

    // Turn 0: birth, defers — transformed messages are the raw input bytes.
    let t0 = run_turn(
        &drift,
        &gate,
        &headers,
        &addr,
        &raw,
        "opus-large",
        "sys",
        true,
    );
    assert_eq!((t0.offloaded, t0.deferred), (0, 1));
    assert_eq!(ser(t0.body["messages"].as_array().unwrap()), raw_base);

    // Turn 1: system drift opens the boundary — the one priced rewrite.
    let t1 = run_turn(
        &drift,
        &gate,
        &headers,
        &addr,
        &raw,
        "opus-large",
        "sys v2",
        true,
    );
    assert_eq!(t1.offloaded, 1);
    let converted: Vec<Vec<u8>> = ser(t1.body["messages"].as_array().unwrap());
    assert_ne!(
        converted[2], raw_base[2],
        "the conversion rewrites exactly once"
    );
    assert_eq!(&converted[..2], &raw_base[..2], "everything else untouched");
    let digest = result_text(&t1.body);

    // Later turns: resends, growth, and a model switch. The converted prefix
    // must re-emit byte-identically; growth appends raw.
    let script = [
        ("opus-large", "sys v2", 0),
        ("opus-large", "sys v2", 1),
        ("sonnet-small", "sys v2", 1),
        ("sonnet-small", "sys v2", 2),
        ("sonnet-small", "sys v2", 3),
    ];
    for (turn, (model, system, grows)) in script.into_iter().enumerate() {
        while raw.len() < history(1).len() + grows * 2 {
            let i = raw.len();
            raw.push(
                json!({"role":"assistant","content":[{"type":"text","text":format!("noted {i}")}]}),
            );
            raw.push(json!({"role":"user","content":[{"type":"text","text":format!("next {i}")}]}));
        }
        let t = run_turn(&drift, &gate, &headers, &addr, &raw, model, system, true);
        assert_eq!(t.offloaded, 1, "turn {turn} ({model}) stays converted");
        let messages = ser(t.body["messages"].as_array().unwrap());
        assert_eq!(
            &messages[..converted.len()],
            &converted[..],
            "turn {turn} ({model}) re-emits the converted prefix byte-identically"
        );
        // Growth tail is raw client bytes, appended after the frozen prefix.
        for (j, msg) in messages[converted.len()..].iter().enumerate() {
            let expected = serde_json::to_vec(&raw[converted.len() + j]).unwrap();
            assert_eq!(msg, &expected, "turn {turn} tail message {j} is raw growth");
        }
        assert_eq!(
            result_text(&t.body),
            digest,
            "turn {turn} ({model}) carries the same digest — including sonnet's first sight"
        );
    }
}
