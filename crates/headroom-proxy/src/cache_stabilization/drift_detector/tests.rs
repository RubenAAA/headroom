use super::*;
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr};

fn anthropic_body(system: &str, tools: serde_json::Value, msgs: Vec<&str>) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = msgs
        .into_iter()
        .map(|t| json!({"role": "user", "content": t}))
        .collect();
    json!({
        "model": "claude-3-5-sonnet-20241022",
        "system": system,
        "tools": tools,
        "messages": messages,
    })
}

fn make_state() -> DriftState {
    DriftState::new(8)
}

/// One user message whose content is an explicit block list, so the
/// block-level attribution has something to index.
fn blocks_body(blocks: serde_json::Value) -> serde_json::Value {
    json!({
        "model": "claude-3-5-sonnet-20241022",
        "system": "you are an assistant",
        "tools": [],
        "messages": [{"role": "user", "content": blocks}],
    })
}

fn detail(prev: &serde_json::Value, curr: &serde_json::Value) -> String {
    early_drift_detail(
        &compute_structural_hash(prev, ApiKind::Anthropic),
        &compute_structural_hash(curr, ApiKind::Anthropic),
    )
}

/// The streaming canonical writer must emit byte-identical bytes to
/// the reference tree builder it replaced. `canonicalize_for_hash`
/// stays as the readable reference; `write_canonical` is the hot
/// path. If this fails, the hashes rotated and every session
/// re-baselines (a full cache-miss burst), so it must not fail
/// silently.
#[test]
fn streaming_canonical_matches_reference_tree() {
    let cases = [
        json!({"z": 1, "a": {"y": [3, 2], "b": "x"}, "cache_control": {"type": "ephemeral"}}),
        json!({"name": "t", "input": {"cache_control": {"type": "ephemeral"}, "q": 1}}),
        json!([{"b": 1, "a": 2}, "s", 1, true, null]),
        json!({"model": "claude-3-7-sonnet[1m]", "system": "sys", "tools": [{"name": "b"}, {"name": "a"}]}),
    ];
    for v in cases {
        let expected = serde_json::to_vec(&canonicalize_for_hash(&v, false)).unwrap();
        let mut actual = Vec::with_capacity(expected.len());
        write_canonical(&mut actual, &v, false).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(
            hash_canonical(&v, false),
            hash_value(&canonicalize_for_hash(&v, false))
        );
        assert_eq!(canonical_len(&v, false), expected.len());
    }
}

/// The case the whole feature exists for: something prepends a
/// block to a settled message. Before this, that read as a bare
/// `early_messages` and named nobody.
#[test]
fn an_inserted_block_reports_the_count_move() {
    let before = blocks_body(json!([{"type": "text", "text": "hello"}]));
    let after = blocks_body(json!([
        {"type": "text", "text": "injected"},
        {"type": "text", "text": "hello"},
    ]));
    assert_eq!(detail(&before, &after), "0:blocks 1->2");
}

#[test]
fn a_rewritten_block_reports_its_index() {
    let before = blocks_body(json!([
        {"type": "text", "text": "stable"},
        {"type": "text", "text": "original"},
    ]));
    let after = blocks_body(json!([
        {"type": "text", "text": "stable"},
        {"type": "text", "text": "rewritten"},
    ]));
    assert_eq!(detail(&before, &after), "0:block[1] text 33B->34B");
}

/// Claude Code relocates its cache breakpoint to the newest block
/// every turn. That is placement, not content — if it registered as
/// a rewritten block the field would be noise on every request.
#[test]
fn a_relocated_breakpoint_is_not_a_block_change() {
    let before = blocks_body(json!([
        {"type": "text", "text": "one", "cache_control": {"type": "ephemeral"}},
        {"type": "text", "text": "two"},
    ]));
    let after = blocks_body(json!([
        {"type": "text", "text": "one"},
        {"type": "text", "text": "two", "cache_control": {"type": "ephemeral"}},
    ]));
    assert_eq!(detail(&before, &after), "", "no drift, so nothing to name");
}

/// A string body used to report `string` — no index, nothing named.
/// It reads as one text block now, which costs nothing and says
/// where the change landed.
#[test]
fn a_string_body_reads_as_the_one_block_it_is() {
    let before = anthropic_body("s", json!([]), vec!["first"]);
    let after = anthropic_body("s", json!([]), vec!["second"]);
    assert_eq!(detail(&before, &after), "0:block[0] text 30B->31B");
}

/// The three most expensive `early_messages` drifts on record (312,861
/// tokens over 2026-08-24/25) all read `2:block[0]` and nothing more, and
/// the bodies were evicted before anyone could ask which block that was.
/// The line now says what the block is and how its size moved, which is
/// what separates a truncation from a rewrite.
#[test]
fn a_rewritten_block_names_its_type_and_size() {
    let before = blocks_body(json!([
        {"type": "tool_result", "tool_use_id": "t1", "content": "the whole file"},
    ]));
    let after = blocks_body(json!([
        {"type": "tool_result", "tool_use_id": "t1", "content": "trunc"},
    ]));
    assert_eq!(
        detail(&before, &after),
        "0:block[0] tool_result 68B->59B",
        "a shrinking tool_result must read as one"
    );
}

/// A block replaced by one of another type is a different defect from a
/// block edited in place, so the line has to distinguish them.
#[test]
fn a_block_that_changed_type_says_so() {
    let before = blocks_body(json!([{"type": "thinking", "thinking": "", "signature": "sig"}]));
    let after = blocks_body(json!([{"type": "text", "text": "hello"}]));
    assert_eq!(detail(&before, &after), "0:block[0] thinking 51B->text 30B");
}

#[test]
fn a_lost_slot_reads_as_dropped() {
    let before = anthropic_body("s", json!([]), vec!["a", "b"]);
    let after = anthropic_body("s", json!([]), vec!["a"]);
    assert_eq!(detail(&before, &after), "1:dropped");
}

/// Growth is append-only and benign — the drift predicate already
/// says so, and the detail must agree rather than blaming a slot.
#[test]
fn growing_into_the_window_names_nothing() {
    let before = anthropic_body("s", json!([]), vec!["a"]);
    let after = anthropic_body("s", json!([]), vec!["a", "b"]);
    assert_eq!(detail(&before, &after), "");
}

/// Beyond the fingerprinted blocks there is no index to report, but
/// the slot must still be named — silence there would look like the
/// stable case.
#[test]
fn a_change_past_the_block_window_still_names_the_slot() {
    let mut before: Vec<serde_json::Value> = (0..EARLY_BLOCKS_WINDOW + 2)
        .map(|i| json!({"type": "text", "text": format!("block {i}")}))
        .collect();
    let mut after = before.clone();
    let last = after.len() - 1;
    after[last] = json!({"type": "text", "text": "changed"});
    before.shrink_to_fit();
    after.shrink_to_fit();
    assert_eq!(
        detail(&blocks_body(json!(before)), &blocks_body(json!(after))),
        format!("0:block[>{}]", EARLY_BLOCKS_WINDOW - 1)
    );
}

#[test]
fn first_request_emits_first_request_event() {
    let state = make_state();
    let body = anthropic_body("you are an assistant", json!([]), vec!["hi"]);
    let h = compute_structural_hash(&body, ApiKind::Anthropic);
    // Before observation: empty cache.
    assert_eq!(state.cache.lock().unwrap().len(), 0);
    observe_drift(&state, "session-A", h);
    // After observation: 1 entry, equal to the input hash.
    let cache = state.cache.lock().unwrap();
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.peek("session-A"), Some(&h));
}

#[test]
fn birth_flag_marks_first_sight_only() {
    // `dims.is_none()` is NOT birth (stable turns also return None) —
    // pin the dedicated signal seeding decisions must use.
    let state = make_state();
    let h1 = compute_structural_hash(
        &anthropic_body("sys-A", json!([]), vec!["m1"]),
        ApiKind::Anthropic,
    );
    let h2 = compute_structural_hash(
        &anthropic_body("sys-B", json!([]), vec!["m1"]),
        ApiKind::Anthropic,
    );
    let (dims, birth) = observe_drift_with_birth(&state, "sess", h1);
    assert_eq!(dims, None);
    assert!(birth, "first sight reports birth");
    let (dims, birth) = observe_drift_with_birth(&state, "sess", h1);
    assert_eq!(dims, None);
    assert!(!birth, "stable turn: no dims AND no birth");
    let (dims, birth) = observe_drift_with_birth(&state, "sess", h2);
    assert!(dims.is_some(), "drifted turn reports dims");
    assert!(!birth, "drifted turn is not a birth");
}

#[test]
fn same_hash_emits_no_event() {
    let state = make_state();
    let body = anthropic_body("sys-A", json!([]), vec!["m1"]);
    let h = compute_structural_hash(&body, ApiKind::Anthropic);
    observe_drift(&state, "sess", h);
    // Second observation with identical hash: still 1 entry, same hash.
    observe_drift(&state, "sess", h);
    let cache = state.cache.lock().unwrap();
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.peek("sess"), Some(&h));
}

#[test]
fn system_drift_detected_with_correct_dim() {
    let state = make_state();
    let h1 = compute_structural_hash(
        &anthropic_body("sys-A", json!([]), vec!["m1"]),
        ApiKind::Anthropic,
    );
    let h2 = compute_structural_hash(
        &anthropic_body("sys-B", json!([]), vec!["m1"]),
        ApiKind::Anthropic,
    );
    assert_ne!(h1.system, h2.system);
    assert_eq!(h1.tools, h2.tools);
    assert_eq!(h1.early_messages, h2.early_messages);
    assert_eq!(drift_dims(&h1, &h2), "system");
    observe_drift(&state, "sess", h1);
    observe_drift(&state, "sess", h2);
}

#[test]
fn tools_drift_detected_with_correct_dim() {
    let h1 = compute_structural_hash(
        &anthropic_body("sys", json!([{"name": "a"}]), vec!["m1"]),
        ApiKind::Anthropic,
    );
    let h2 = compute_structural_hash(
        &anthropic_body("sys", json!([{"name": "b"}]), vec!["m1"]),
        ApiKind::Anthropic,
    );
    assert_eq!(h1.system, h2.system);
    assert_ne!(h1.tools, h2.tools);
    assert_eq!(h1.early_messages, h2.early_messages);
    assert_eq!(drift_dims(&h1, &h2), "tools");
}

/// Claude Code with its ToolSearch on: a placeholder deferred on every
/// request, and each loaded tool appended still `defer_loading: true`.
fn deferred_tools(loaded: &[&str]) -> serde_json::Value {
    let mut tools = vec![
        json!({"name": "Read", "input_schema": {"type": "object"}}),
        json!({"name": "ToolSearch", "input_schema": {"type": "object"}}),
        json!({"name": "DeferredToolPlaceholder", "input_schema": {"type": "object"}, "defer_loading": true}),
    ];
    tools.extend(loaded.iter().map(
        |name| json!({"name": name, "input_schema": {"type": "object"}, "defer_loading": true}),
    ));
    serde_json::Value::Array(tools)
}

#[test]
fn deferred_tool_load_is_not_drift_when_it_reaches_the_provider_deferred() {
    let before = anthropic_body("sys", deferred_tools(&[]), vec!["m1"]);
    let after = anthropic_body("sys", deferred_tools(&["Monitor"]), vec!["m1", "m2"]);
    let state = DriftState::new(8);
    observe_drift(
        &state,
        "lane",
        compute_structural_hash_with(&before, ApiKind::Anthropic, true),
    );
    assert_eq!(
        observe_drift(
            &state,
            "lane",
            compute_structural_hash_with(&after, ApiKind::Anthropic, true),
        ),
        None,
        "the provider keys its cache without deferred tools; a load is not a rebuild"
    );

    // Third-party upstreams get every tool eager: there the load is drift.
    let h1 = compute_structural_hash_with(&before, ApiKind::Anthropic, false);
    let h2 = compute_structural_hash_with(&after, ApiKind::Anthropic, false);
    assert_eq!(drift_dims(&h1, &h2), "tools");
}

#[test]
fn eager_tools_and_the_first_deferred_tool_still_drift_when_unkeyed() {
    let hash = |tools: serde_json::Value| {
        compute_structural_hash_with(
            &anthropic_body("sys", tools, vec!["m1"]),
            ApiKind::Anthropic,
            true,
        )
    };
    let base = hash(deferred_tools(&["Monitor"]));
    // An eager tool is rendered into the prompt.
    let mut added = deferred_tools(&["Monitor"]);
    added
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "Bash", "input_schema": {"type": "object"}}));
    assert_eq!(drift_dims(&base, &hash(added)), "tools");
    // Un-deferring a loaded tool renders it too.
    let mut undeferred = deferred_tools(&["Monitor"]);
    undeferred[3]["defer_loading"] = json!(false);
    assert_eq!(drift_dims(&base, &hash(undeferred)), "tools");
    // No deferred tool at all, then one: the deferred-tool section appears.
    let eager_only = json!([{"name": "Read", "input_schema": {"type": "object"}}]);
    let mut first_deferred = eager_only.clone();
    first_deferred.as_array_mut().unwrap().push(
        json!({"name": "Monitor", "input_schema": {"type": "object"}, "defer_loading": true}),
    );
    assert_eq!(
        drift_dims(&hash(eager_only), &hash(first_deferred)),
        "tools"
    );
}

#[test]
fn early_messages_drift_detected_with_correct_dim() {
    let h1 = compute_structural_hash(
        &anthropic_body("sys", json!([]), vec!["m1"]),
        ApiKind::Anthropic,
    );
    let h2 = compute_structural_hash(
        &anthropic_body("sys", json!([]), vec!["DIFFERENT"]),
        ApiKind::Anthropic,
    );
    assert_eq!(h1.system, h2.system);
    assert_eq!(h1.tools, h2.tools);
    assert_ne!(h1.early_messages, h2.early_messages);
    assert_eq!(drift_dims(&h1, &h2), "early_messages");
}

#[test]
fn multi_dim_drift_lists_all_changed_dims() {
    let h1 = compute_structural_hash(
        &anthropic_body("sys-A", json!([{"name": "a"}]), vec!["m1"]),
        ApiKind::Anthropic,
    );
    let h2 = compute_structural_hash(
        &anthropic_body("sys-B", json!([{"name": "b"}]), vec!["X"]),
        ApiKind::Anthropic,
    );
    assert_eq!(drift_dims(&h1, &h2), "system,tools,early_messages");
}

#[test]
fn lru_evicts_at_capacity() {
    // Capacity 2: inserting a 3rd session evicts the LRU.
    let state = DriftState::new(2);
    let h = compute_structural_hash(
        &anthropic_body("s", json!([]), vec!["m"]),
        ApiKind::Anthropic,
    );
    observe_drift(&state, "s1", h);
    observe_drift(&state, "s2", h);
    observe_drift(&state, "s3", h);
    let cache = state.cache.lock().unwrap();
    assert_eq!(cache.len(), 2);
    // s1 was the least-recently-used; should have been evicted.
    assert!(!cache.contains("s1"));
    assert!(cache.contains("s2"));
    assert!(cache.contains("s3"));
}

/// The live signature this was built for: a commit SHA rotating in
/// the system prompt. Same length, different content — which is
/// what separates a volatile field from a client editing its
/// instructions, and the reason `line_lens` sits next to the hash.
#[test]
fn a_fixed_width_field_changing_is_named_with_an_unchanged_length() {
    let before = serde_json::json!("instructions\ncommit abc1234\nmore");
    let after = serde_json::json!("instructions\ncommit def5678\nmore");
    let detail = system_drift_detail(&system_shape(&before), &system_shape(&after));
    assert_eq!(detail, "line[1]:14->14");
}

/// Text arriving on a settled line reads as a length delta, so an
/// operator can tell "18 bytes appeared" from "a SHA rotated".
#[test]
fn text_arriving_on_a_line_is_named_with_its_growth() {
    let before = serde_json::json!("instructions\nstatus: clean\nmore");
    let after = serde_json::json!("instructions\nstatus: 3 files modified\nmore");
    let detail = system_drift_detail(&system_shape(&before), &system_shape(&after));
    assert_eq!(detail, "line[1]:13->24");
}

#[test]
fn lines_appearing_are_counted_as_well_as_named() {
    let before = serde_json::json!("a\nb");
    let after = serde_json::json!("a\nb\nc");
    let detail = system_drift_detail(&system_shape(&before), &system_shape(&after));
    assert_eq!(detail, "lines 2->3,line[2]:0->1");
}

#[test]
fn a_system_block_that_held_still_says_nothing() {
    let system = serde_json::json!([
        {"type": "text", "text": "instructions\nmore"},
    ]);
    let detail = system_drift_detail(&system_shape(&system), &system_shape(&system));
    assert!(detail.is_empty(), "{detail}");
}

/// A breakpoint moving between system blocks must not read as a
/// rewritten prompt: only `text` is fingerprinted.
#[test]
fn a_relocated_breakpoint_does_not_read_as_system_drift() {
    let before = serde_json::json!([
        {"type": "text", "text": "one", "cache_control": {"type": "ephemeral"}},
        {"type": "text", "text": "two"},
    ]);
    let after = serde_json::json!([
        {"type": "text", "text": "one"},
        {"type": "text", "text": "two", "cache_control": {"type": "ephemeral"}},
    ]);
    let detail = system_drift_detail(&system_shape(&before), &system_shape(&after));
    assert!(detail.is_empty(), "{detail}");
}

/// Blocks joining with a newline means a two-block system and the
/// same text as one block index their lines identically.
#[test]
fn blocks_and_one_string_index_the_same_lines() {
    let split = serde_json::json!([
        {"type": "text", "text": "one"},
        {"type": "text", "text": "two"},
    ]);
    let joined = serde_json::json!("one\ntwo");
    assert_eq!(system_shape(&split), system_shape(&joined));
}

#[test]
fn a_wholesale_rewrite_names_a_few_lines_and_then_stops() {
    let before = serde_json::json!("a\nb\nc\nd\ne\nf");
    let after = serde_json::json!("A\nB\nC\nD\nE\nF");
    let detail = system_drift_detail(&system_shape(&before), &system_shape(&after));
    assert!(detail.ends_with(",..."), "{detail}");
    assert_eq!(detail.matches("line[").count(), 4, "{detail}");
}

/// Past the window the detail degrades to a marker rather than
/// reporting "nothing changed" for a system block that did.
#[test]
fn a_change_past_the_window_is_reported_without_an_index() {
    let mut before: Vec<String> = (0..SYSTEM_LINES_WINDOW + 2)
        .map(|i| format!("line {i}"))
        .collect();
    let after_text = before.join("\n");
    before[SYSTEM_LINES_WINDOW + 1] = "changed past the window".to_string();
    let detail = system_drift_detail(
        &system_shape(&serde_json::json!(before.join("\n"))),
        &system_shape(&serde_json::json!(after_text)),
    );
    assert_eq!(detail, format!("line[>{SYSTEM_LINES_WINDOW}]"));
}

/// An empty line must not read as an absent one, or a blank line
/// arriving would be invisible.
#[test]
fn an_empty_line_is_hashed_rather_than_read_as_absent() {
    let shape = system_shape(&serde_json::json!("a\n\nb"));
    assert_eq!(shape.lines, 3);
    assert_ne!(shape.line_hashes[1], 0);
}

#[test]
fn does_not_mutate_input() {
    let body = anthropic_body(
        "sys",
        json!([{"name": "t1", "input_schema": {"type": "object"}}]),
        vec!["m1", "m2", "m3", "m4"],
    );
    let original_bytes = serde_json::to_vec(&body).expect("serialize");
    // Compute the hash twice — across the three ApiKind shapes —
    // to exercise every branch that *could* mutate the input.
    let _ = compute_structural_hash(&body, ApiKind::Anthropic);
    let _ = compute_structural_hash(&body, ApiKind::OpenAiChat);
    let _ = compute_structural_hash(&body, ApiKind::OpenAiResponses);
    let after_bytes = serde_json::to_vec(&body).expect("re-serialize");
    assert_eq!(original_bytes, after_bytes);
}

#[test]
fn session_key_hashes_authorization_does_not_log_raw() {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer sk-ant-very-secret-token-do-not-log-me"
            .parse()
            .unwrap(),
    );
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 1234);
    let key = derive_session_key(&headers, &addr, &json!({}), ApiKind::Anthropic);
    // The key MUST NOT contain the raw bearer string anywhere —
    // not the secret token, not the literal "Bearer", not even
    // any 8+ char substring of the secret.
    assert!(
        !key.contains("sk-ant"),
        "session key leaked raw secret prefix: {key}"
    );
    assert!(
        !key.contains("very-secret"),
        "session key leaked raw secret middle: {key}"
    );
    assert!(
        !key.contains("Bearer"),
        "session key leaked the auth scheme: {key}"
    );
    // The key SHOULD be the auth-scoped envelope, so we know the
    // `Authorization` arm was taken (not the IP fallback).
    assert!(key.starts_with("auth:"), "expected auth-scoped key: {key}");
    // And the log prefix must also not leak the raw secret.
    let log_prefix = session_key_log_prefix(&key);
    assert!(!log_prefix.contains("sk-ant"));
    assert!(!log_prefix.contains("very-secret"));
    assert!(!log_prefix.contains("Bearer"));
    assert_eq!(log_prefix.len(), 32); // 16 bytes × 2 hex chars
}

#[test]
fn session_key_hashes_x_api_key_does_not_log_raw() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-api-key",
        "sk-very-private-api-key-12345".parse().unwrap(),
    );
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 1234);
    let key = derive_session_key(&headers, &addr, &json!({}), ApiKind::Anthropic);
    assert!(!key.contains("sk-very-private"));
    assert!(key.starts_with("apikey:"));
}

#[test]
fn session_key_falls_back_to_ip_then_ip_ua() {
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)), 5555);
    // No headers → ip-only.
    let bare = derive_session_key(&HeaderMap::new(), &addr, &json!({}), ApiKind::Anthropic);
    assert!(bare.starts_with("ip:"));
    // With UA → ipua-tuple.
    let mut headers = HeaderMap::new();
    headers.insert(axum::http::header::USER_AGENT, "ua-test".parse().unwrap());
    let with_ua = derive_session_key(&headers, &addr, &json!({}), ApiKind::Anthropic);
    assert!(with_ua.starts_with("ipua:"));
    assert_ne!(bare, with_ua);
}

/// C8 frozen vector: byte-exact session-key output for a fixed input.
/// Any derivation change fails loudly here and forces versioned keys +
/// dual-read migration — silent drift invalidates every stored session
/// at once (fleet-wide hit-rate cliff, zero errors). Do NOT update the
/// literal to match new code; version the keys instead.
#[test]
fn session_key_ip_fallback_vector_is_frozen() {
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)), 5555);
    let bare = derive_session_key(&HeaderMap::new(), &addr, &json!({}), ApiKind::Anthropic);
    assert_eq!(bare, "ip:10.0.0.3:-");
}

/// C8 frozen vector: the lane key folds the system hash into the
/// session key, so same-opener streams on different systems stop
/// overwriting each other's drift baseline.
#[test]
fn stream_lane_key_vector_is_frozen() {
    let body = anthropic_body("sys-lane", json!([]), vec!["m1"]);
    let hash = compute_structural_hash(&body, ApiKind::Anthropic);
    let lane = stream_lane_key("ip:10.0.0.3:-", &hash);
    assert_eq!(lane, "ip:10.0.0.3:-\u{1f}a2c1cfc3ffc873c7");
    // The separator is the control char no session-key arm emits.
    assert!(lane.starts_with("ip:10.0.0.3:-\u{1f}"));
    assert_eq!(lane_session_part(&lane), "ip:10.0.0.3:-");
    // A system flip is a lane switch, not an invalidation.
    let flipped = anthropic_body("sys-other", json!([]), vec!["m1"]);
    let flipped_hash = compute_structural_hash(&flipped, ApiKind::Anthropic);
    assert_ne!(stream_lane_key("ip:10.0.0.3:-", &flipped_hash), lane);
}

/// C8 frozen vector: the cross-model lineage key is (identity branch,
/// full message hash) with no model folded in, so a model switch finds
/// its donor session.
#[test]
fn model_free_lineage_key_vector_is_frozen() {
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)), 5555);
    let body = anthropic_body("sys-any", json!([]), vec!["opener"]);
    let lineage = model_free_lineage_key(&HeaderMap::new(), &addr, &body, ApiKind::Anthropic);
    let (branch, msg_hash) = lineage.expect("ip identity with messages");
    assert_eq!(branch, "ip:10.0.0.3");
    assert_eq!(
        msg_hash,
        "d9c8804af9d1726f18855e942e68dd1c1fc55de54671f7367f438bdfc7cff92d"
    );
    assert_eq!(
        msg_hash.len(),
        64,
        "full hash, not the 8-hex session prefix"
    );
    // An explicitly pinned session is already shared: no lineage lookup.
    let mut pinned = HeaderMap::new();
    pinned.insert("x-headroom-session-id", "operator-session".parse().unwrap());
    assert!(model_free_lineage_key(&pinned, &addr, &body, ApiKind::Anthropic).is_none());
}

#[test]
fn openai_chat_extracts_first_system_message() {
    let body = json!({
        "model": "gpt-4",
        "messages": [
            {"role": "system", "content": "you are a helpful assistant"},
            {"role": "user", "content": "hi"},
        ],
        "tools": [],
    });
    let h1 = compute_structural_hash(&body, ApiKind::OpenAiChat);
    // Same body but a different system message → system axis drifts.
    let body2 = json!({
        "model": "gpt-4",
        "messages": [
            {"role": "system", "content": "you are a different assistant"},
            {"role": "user", "content": "hi"},
        ],
        "tools": [],
    });
    let h2 = compute_structural_hash(&body2, ApiKind::OpenAiChat);
    assert_ne!(h1.system, h2.system);
    // user message identical → early-messages stays identical.
    assert_eq!(h1.early_messages, h2.early_messages);
}

#[test]
fn openai_responses_uses_instructions_and_input() {
    let body = json!({
        "model": "gpt-4",
        "instructions": "be brief",
        "tools": [],
        "input": [
            {"type": "message", "role": "user", "content": "hello"},
        ],
    });
    let h1 = compute_structural_hash(&body, ApiKind::OpenAiResponses);
    let body2 = json!({
        "model": "gpt-4",
        "instructions": "be verbose",
        "tools": [],
        "input": [
            {"type": "message", "role": "user", "content": "hello"},
        ],
    });
    let h2 = compute_structural_hash(&body2, ApiKind::OpenAiResponses);
    assert_ne!(h1.system, h2.system);
    assert_eq!(h1.early_messages, h2.early_messages);
}

/// Shape observed from live Claude Code traffic: turn 1 sends a single
/// user message whose *last* block carries the `cache_control` marker;
/// on turn 2 the same message returns byte-identical except the marker
/// moved to the newest message's first block.
fn cc_turn1_body() -> serde_json::Value {
    json!({
        "model": "claude-haiku-4-5",
        "system": [{"type": "text", "text": "agent preamble"}],
        "tools": [{"name": "bash"}],
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "opening prompt"},
                {"type": "text", "text": "project context", "cache_control": {"type": "ephemeral"}},
            ]},
        ],
    })
}

fn cc_turn2_body() -> serde_json::Value {
    json!({
        "model": "claude-haiku-4-5",
        "system": [{"type": "text", "text": "agent preamble"}],
        "tools": [{"name": "bash"}],
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "opening prompt"},
                {"type": "text", "text": "project context"},
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": "reply"}]},
            {"role": "user", "content": [
                {"type": "text", "text": "second prompt", "cache_control": {"type": "ephemeral"}},
            ]},
        ],
    })
}

#[test]
fn different_conversations_on_one_credential_get_distinct_session_keys() {
    // Interactive clients (Claude Code, Codex CLI) send the same
    // `Authorization` bearer for every concurrent conversation. If the
    // session key stops at the credential, their alternating requests
    // ping-pong one LRU slot and every switch logs a false
    // `cache_drift_observed`.
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer shared-workspace-token".parse().unwrap(),
    );
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
    let conv_a = anthropic_body("sys", json!([]), vec!["conversation A opener"]);
    let conv_b = anthropic_body("sys", json!([]), vec!["conversation B opener"]);
    let key_a = derive_session_key(&headers, &addr, &conv_a, ApiKind::Anthropic);
    let key_b = derive_session_key(&headers, &addr, &conv_b, ApiKind::Anthropic);
    assert_ne!(
        key_a, key_b,
        "two conversations sharing one credential must not share a drift session"
    );
}

#[test]
fn same_conversation_next_turn_keeps_its_session_key() {
    // The discriminator must survive normal turn-to-turn growth: the
    // opener is byte-identical on turn 2 except its relocated
    // `cache_control` marker.
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer shared-workspace-token".parse().unwrap(),
    );
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
    let key_t1 = derive_session_key(&headers, &addr, &cc_turn1_body(), ApiKind::Anthropic);
    let key_t2 = derive_session_key(&headers, &addr, &cc_turn2_body(), ApiKind::Anthropic);
    assert_eq!(
        key_t1, key_t2,
        "turn growth and cache_control relocation must not rotate the session key"
    );
}

/// 2026-09-08: same-opener subagent streams share a session key while
/// carrying different systems. The lane must split them.
#[test]
fn lane_key_splits_streams_sharing_a_session() {
    let session = "auth:abc123:conv456";
    let hash_a = compute_structural_hash(&cc_turn1_body(), ApiKind::Anthropic);
    let mut other = cc_turn1_body();
    other["system"] = json!([{"type": "text", "text": "subagent preamble"}]);
    let hash_b = compute_structural_hash(&other, ApiKind::Anthropic);
    let lane_a = stream_lane_key(session, &hash_a);
    let lane_b = stream_lane_key(session, &hash_b);
    assert_ne!(lane_a, lane_b, "one system per lane");
    assert_eq!(lane_session_part(&lane_a), session);
    assert_eq!(lane_session_part(&lane_b), session);
}

#[test]
fn lane_key_survives_growth_and_ignores_tools() {
    let session = "auth:abc123:conv456";
    let h1 = compute_structural_hash(&cc_turn1_body(), ApiKind::Anthropic);
    let h2 = compute_structural_hash(&cc_turn2_body(), ApiKind::Anthropic);
    assert_eq!(
        stream_lane_key(session, &h1),
        stream_lane_key(session, &h2),
        "append-only growth must not rotate the lane"
    );
    let mut retooled = cc_turn2_body();
    retooled["tools"] = json!([{"name": "bash"}, {"name": "read"}]);
    let h3 = compute_structural_hash(&retooled, ApiKind::Anthropic);
    assert_eq!(
        stream_lane_key(session, &h2),
        stream_lane_key(session, &h3),
        "a mid-conversation tool change must not rotate the lane"
    );
}

#[test]
fn lane_session_part_passes_bare_keys_through() {
    assert_eq!(lane_session_part("auth:abc"), "auth:abc");
    assert_eq!(lane_session_part(""), "");
}

/// The invariant every lane consumer depends on: two same-opener streams
/// with different systems share one session key (the discriminator is
/// system-blind on purpose) but mint different lanes. Drift, usage and
/// replay take the lane; ctx/memory stores keep the session, so sibling
/// streams split caches without splitting recall.
#[test]
fn sibling_streams_share_the_session_but_not_the_lane() {
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "sk-test".parse().unwrap());
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
    let main = anthropic_body("main instructions", json!([]), vec!["shared opener"]);
    let subagent = anthropic_body("subagent preamble", json!([]), vec!["shared opener"]);

    let session_main = derive_session_key(&headers, &addr, &main, ApiKind::Anthropic);
    let session_sub = derive_session_key(&headers, &addr, &subagent, ApiKind::Anthropic);
    assert_eq!(
        session_main, session_sub,
        "the session discriminator must not see the system prompt"
    );

    let lane_main = stream_lane_key(
        &session_main,
        &compute_structural_hash(&main, ApiKind::Anthropic),
    );
    let lane_sub = stream_lane_key(
        &session_sub,
        &compute_structural_hash(&subagent, ApiKind::Anthropic),
    );
    assert_ne!(
        lane_main, lane_sub,
        "different systems must mint different lanes"
    );
    assert_eq!(lane_session_part(&lane_main), session_main);
    assert_eq!(lane_session_part(&lane_sub), session_sub);
}

/// The incident shape: A, B, A turns of one session must not read as
/// drift. Return-value proof — `Some` means a `cache_drift_observed`
/// warn plus (downstream) a dropped stored prefix.
#[test]
fn alternating_lanes_do_not_drift() {
    let state = DriftState::new(8);
    let session = "auth:abc123:conv456";
    let hash_a = compute_structural_hash(&cc_turn1_body(), ApiKind::Anthropic);
    let mut body_b = cc_turn1_body();
    body_b["system"] = json!([{"type": "text", "text": "subagent preamble"}]);
    let hash_b = compute_structural_hash(&body_b, ApiKind::Anthropic);
    let lane_a = stream_lane_key(session, &hash_a);
    let lane_b = stream_lane_key(session, &hash_b);
    assert_eq!(observe_drift(&state, &lane_a, hash_a), None);
    assert_eq!(observe_drift(&state, &lane_b, hash_b), None);
    // Back to A with growth: steady within its own lane.
    let hash_a2 = compute_structural_hash(&cc_turn2_body(), ApiKind::Anthropic);
    // cc_turn2 shares cc_turn1's system, so it stays on lane A.
    assert_eq!(observe_drift(&state, &lane_a, hash_a2), None);
}

#[test]
fn explicit_headroom_session_header_wins_over_credentials() {
    // The Python proxy honors `x-headroom-session-id` as the highest-
    // priority session identity (prefix tracker, beta-header tracker);
    // the drift detector must respect the same explicit opt-in.
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer shared-workspace-token".parse().unwrap(),
    );
    headers.insert("x-headroom-session-id", "conv-42".parse().unwrap());
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
    let body_a = anthropic_body("sys", json!([]), vec!["conversation A opener"]);
    let body_b = anthropic_body("sys", json!([]), vec!["conversation B opener"]);
    let key_a = derive_session_key(&headers, &addr, &body_a, ApiKind::Anthropic);
    assert!(
        key_a.starts_with("session:"),
        "explicit session header must define the key, got: {key_a}"
    );
    assert!(
        !key_a.contains("conv-42"),
        "session keys stay opaque even for non-secret ids: {key_a}"
    );
    // The explicit id pins the session across body variance…
    let key_b = derive_session_key(&headers, &addr, &body_b, ApiKind::Anthropic);
    assert_eq!(key_a, key_b);
    // …and different ids mean different sessions.
    let mut headers2 = headers.clone();
    headers2.insert("x-headroom-session-id", "conv-43".parse().unwrap());
    let key_c = derive_session_key(&headers2, &addr, &body_a, ApiKind::Anthropic);
    assert_ne!(key_a, key_c);
}

#[test]
fn cache_control_relocation_and_growth_are_not_drift() {
    // Turn 1 → turn 2 of a single conversation: the early window gains
    // messages and the client relocates its `cache_control` marker to
    // the newest block. Neither busts the provider's prefix cache, so
    // neither is drift.
    let h1 = compute_structural_hash(&cc_turn1_body(), ApiKind::Anthropic);
    let h2 = compute_structural_hash(&cc_turn2_body(), ApiKind::Anthropic);
    assert_eq!(
        drift_dims(&h1, &h2),
        "",
        "append-only growth with marker relocation must not be drift"
    );
}

#[test]
fn a_breakpoint_arriving_on_a_settled_message_is_not_drift() {
    // Claude Code sends a message as a bare string, then wraps the
    // same text in a block so it can hang a breakpoint on it. Same
    // text, same billed tokens, same prefix — the wrapper is how the
    // marker attaches, not a rewrite.
    let plain = json!({"model": "m", "system": "s", "tools": [], "messages": [
        {"role": "user", "content": "settled turn"},
    ]});
    let wrapped = json!({"model": "m", "system": "s", "tools": [], "messages": [
        {"role": "user", "content": [
            {"type": "text", "text": "settled turn",
             "cache_control": {"type": "ephemeral"}},
        ]},
    ]});
    let h1 = compute_structural_hash(&plain, ApiKind::Anthropic);
    let h2 = compute_structural_hash(&wrapped, ApiKind::Anthropic);
    assert_eq!(
        drift_dims(&h1, &h2),
        "",
        "wrapping text to carry a breakpoint must not read as a rewrite"
    );
    // And back again when the marker moves on.
    assert_eq!(drift_dims(&h2, &h1), "");
}

#[test]
fn a_rewrite_under_a_breakpoint_is_still_drift() {
    // The collapse must not blind the detector: same wrapper, new
    // text, and that is a genuine bust.
    let before = json!({"model": "m", "system": "s", "tools": [], "messages": [
        {"role": "user", "content": "settled turn"},
    ]});
    let after = json!({"model": "m", "system": "s", "tools": [], "messages": [
        {"role": "user", "content": [
            {"type": "text", "text": "REWRITTEN",
             "cache_control": {"type": "ephemeral"}},
        ]},
    ]});
    let h1 = compute_structural_hash(&before, ApiKind::Anthropic);
    let h2 = compute_structural_hash(&after, ApiKind::Anthropic);
    assert_eq!(drift_dims(&h1, &h2), "early_messages");
}

#[test]
fn a_dropped_reminder_block_is_no_longer_drift() {
    // Reversed on 2026-08-26. This case was `early_messages` drift, on
    // 2026-08-11 data, and that was right at the time: the relocation pass
    // normalised reminders away for every subsystem, so one reaching the
    // hash meant something else had moved.
    //
    // Relocation was removed on 2026-08-16 — it cost 64% of all billed
    // weight — and `prefix_replay` alone was given the job of absorbing
    // this churn. But drift runs first and drops the stored prefix, so the
    // replay that would have preserved the cache never got to see the turn.
    // The withdrawal was free at message 47 and cost a full re-cache at
    // message 1: 5 events, 430,296 tokens, 2026-08-23/26, every one within
    // 137s of the previous turn against a 1h cache TTL.
    let with_reminder = json!({"model": "m", "system": "s", "tools": [], "messages": [
        {"role": "user", "content": [{"type": "text", "text": "opener"}]},
        {"role": "user", "content": [
            {"type": "text", "text": "real turn"},
            {"type": "text", "text": "<system-reminder>hook output</system-reminder>"},
        ]},
    ]});
    let without = json!({"model": "m", "system": "s", "tools": [], "messages": [
        {"role": "user", "content": [{"type": "text", "text": "opener"}]},
        {"role": "user", "content": [{"type": "text", "text": "real turn"}]},
    ]});
    let h1 = compute_structural_hash(&with_reminder, ApiKind::Anthropic);
    let h2 = compute_structural_hash(&without, ApiKind::Anthropic);
    assert_eq!(
        drift_dims(&h1, &h2),
        "",
        "a withdrawn reminder must not read as a rebuild boundary"
    );
}

#[test]
fn a_dropped_real_block_is_drift_and_names_the_slot() {
    // The other half of the test above, and the reason the detail exists:
    // a block that is not scaffolding still moves the count and still
    // names the slot. Two blocks never collapse, so the structure holds.
    let with_block = json!({"model": "m", "system": "s", "tools": [], "messages": [
        {"role": "user", "content": [{"type": "text", "text": "opener"}]},
        {"role": "user", "content": [
            {"type": "text", "text": "real turn"},
            {"type": "text", "text": "a second thing the user said"},
        ]},
    ]});
    let without = json!({"model": "m", "system": "s", "tools": [], "messages": [
        {"role": "user", "content": [{"type": "text", "text": "opener"}]},
        {"role": "user", "content": [{"type": "text", "text": "real turn"}]},
    ]});
    let h1 = compute_structural_hash(&with_block, ApiKind::Anthropic);
    let h2 = compute_structural_hash(&without, ApiKind::Anthropic);
    assert_eq!(drift_dims(&h1, &h2), "early_messages");
    assert_eq!(
        early_drift_detail(&h1, &h2),
        "1:blocks 2->1",
        "the detail must name the slot and the block count that moved"
    );
}

#[test]
fn a_withdrawn_scaffolding_message_does_not_shift_the_window() {
    // The costliest shape in the logs: the standalone `role: "system"`
    // bare-string reminder, held at index 1 by 34 of 87 stored prefixes on
    // 2026-08-26. Withdrawing it used to slide every later message up a
    // slot, which the detector read as two rewritten messages at once —
    // logged as `1:blocks 1->2,2:blocks 2->1`.
    let with_scaffolding = json!({"model": "m", "system": "s", "tools": [], "messages": [
        {"role": "user", "content": [{"type": "text", "text": "opener"}]},
        {"role": "system", "content": "<system-reminder>skills available</system-reminder>"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "..."},
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {}},
        ]},
    ]});
    let without = json!({"model": "m", "system": "s", "tools": [], "messages": [
        {"role": "user", "content": [{"type": "text", "text": "opener"}]},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "..."},
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {}},
        ]},
    ]});
    let h1 = compute_structural_hash(&with_scaffolding, ApiKind::Anthropic);
    let h2 = compute_structural_hash(&without, ApiKind::Anthropic);
    assert_eq!(
        drift_dims(&h1, &h2),
        "",
        "withdrawing a scaffolding-only message must not shift the window"
    );
}

#[test]
fn append_only_growth_without_markers_is_not_drift() {
    let h1 = compute_structural_hash(
        &anthropic_body("s", json!([]), vec!["m1"]),
        ApiKind::Anthropic,
    );
    let h2 = compute_structural_hash(
        &anthropic_body("s", json!([]), vec!["m1", "m2", "m3"]),
        ApiKind::Anthropic,
    );
    assert_eq!(
        drift_dims(&h1, &h2),
        "",
        "a conversation growing into the early window must not be drift"
    );
}

#[test]
fn rewritten_early_message_is_still_drift() {
    let h1 = compute_structural_hash(
        &anthropic_body("s", json!([]), vec!["m1", "m2", "m3"]),
        ApiKind::Anthropic,
    );
    let h2 = compute_structural_hash(
        &anthropic_body("s", json!([]), vec!["REWRITTEN", "m2", "m3"]),
        ApiKind::Anthropic,
    );
    assert_eq!(drift_dims(&h1, &h2), "early_messages");
}

#[test]
fn shrunk_history_is_still_drift() {
    // Fewer messages than previously observed *under the same
    // session key* means the settled prefix was rewritten in place
    // — that IS a cache bust. (Reachable when the identity is
    // pinned, e.g. an explicit x-headroom-session-id; on the
    // credential arms a first-message rewrite re-keys instead —
    // see conversation_discriminator's trade-off note.)
    let h1 = compute_structural_hash(
        &anthropic_body("s", json!([]), vec!["m1", "m2", "m3"]),
        ApiKind::Anthropic,
    );
    let h2 = compute_structural_hash(
        &anthropic_body("s", json!([]), vec!["m1"]),
        ApiKind::Anthropic,
    );
    assert_eq!(drift_dims(&h1, &h2), "early_messages");
}

#[test]
fn same_opener_on_different_model_gets_distinct_session_keys() {
    // Auxiliary small-model calls (title generation, summaries)
    // reuse a conversation's opener under the same credential.
    // Provider prompt caches are per-model, so these are separate
    // cache lineages and must not share a drift baseline — the
    // sidecar's different system prompt would otherwise false-warn.
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer shared-workspace-token".parse().unwrap(),
    );
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
    let mut main_conv = anthropic_body("sys", json!([]), vec!["shared opener"]);
    let mut sidecar = anthropic_body("sys", json!([]), vec!["shared opener"]);
    main_conv["model"] = json!("opus-large");
    sidecar["model"] = json!("haiku-small");
    let key_main = derive_session_key(&headers, &addr, &main_conv, ApiKind::Anthropic);
    let key_side = derive_session_key(&headers, &addr, &sidecar, ApiKind::Anthropic);
    assert_ne!(key_main, key_side);
}

#[test]
fn lineage_key_matches_across_models_not_conversations() {
    // The seeding index: same credential + same opener match whatever
    // the model; anything else must not.
    fn headers_for(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::AUTHORIZATION, token.parse().unwrap());
        headers
    }
    fn addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242)
    }
    let mut opus = anthropic_body("sys", json!([]), vec!["shared opener"]);
    let mut sonnet = anthropic_body("sys", json!([]), vec!["shared opener"]);
    opus["model"] = json!("opus-large");
    sonnet["model"] = json!("sonnet-small");
    let headers = headers_for("Bearer shared-workspace-token");
    let ko = model_free_lineage_key(&headers, &addr(), &opus, ApiKind::Anthropic).expect("lineage");
    let ks =
        model_free_lineage_key(&headers, &addr(), &sonnet, ApiKind::Anthropic).expect("lineage");
    assert_eq!(ko, ks, "same credential+opener seeds across models");
    // Full 64-hex message half: session prefixes are 8 hex, the index
    // must not conflate distinct conversations.
    assert_eq!(ko.1.len(), 64);

    // Different opener → different lineage.
    let mut other = anthropic_body("sys", json!([]), vec!["other opener"]);
    other["model"] = json!("sonnet-small");
    let kx =
        model_free_lineage_key(&headers, &addr(), &other, ApiKind::Anthropic).expect("lineage");
    assert_ne!(ks, kx);

    // Different credential → different lineage.
    let headers2 = headers_for("Bearer other-token");
    let ky =
        model_free_lineage_key(&headers2, &addr(), &sonnet, ApiKind::Anthropic).expect("lineage");
    assert_ne!(ks, ky);

    // Explicit session id → None: operator-managed identity already
    // shares all state, nothing to seed.
    let mut pinned = headers.clone();
    pinned.insert(
        "x-headroom-session-id"
            .parse::<axum::http::HeaderName>()
            .unwrap(),
        "pinned".parse().unwrap(),
    );
    assert_eq!(
        model_free_lineage_key(&pinned, &addr(), &sonnet, ApiKind::Anthropic),
        None
    );

    // No conversation messages → None.
    let empty = anthropic_body("sys", json!([]), vec![]);
    assert_eq!(
        model_free_lineage_key(&headers, &addr(), &empty, ApiKind::Anthropic),
        None
    );
}

#[test]
fn key_order_variation_does_not_perturb_hashes_or_identity() {
    // The workspace's serde_json enables `preserve_order`, so two
    // serializations of the same message with different key order
    // stay distinct through Value round-trips. Canonicalization
    // must neutralize that for both the axes and the session key.
    let body_a: serde_json::Value = serde_json::from_str(
        r#"{"model":"m","system":"s","tools":[],
                "messages":[{"role":"user","content":"hello"}]}"#,
    )
    .unwrap();
    let body_b: serde_json::Value = serde_json::from_str(
        r#"{"model":"m","system":"s","tools":[],
                "messages":[{"content":"hello","role":"user"}]}"#,
    )
    .unwrap();
    assert_eq!(
        compute_structural_hash(&body_a, ApiKind::Anthropic),
        compute_structural_hash(&body_b, ApiKind::Anthropic),
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer shared-workspace-token".parse().unwrap(),
    );
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
    assert_eq!(
        derive_session_key(&headers, &addr, &body_a, ApiKind::Anthropic),
        derive_session_key(&headers, &addr, &body_b, ApiKind::Anthropic),
    );
}

#[test]
fn cache_control_named_fields_inside_opaque_payloads_still_count() {
    // A tool schema property (or tool-call argument) that happens
    // to be NAMED cache_control is user data, not a cache marker.
    // Changing it must still read as drift on the affected axis.
    let with_schema = |ty: &str| {
        json!({
            "model": "m", "system": "s",
            "tools": [{"name": "t", "input_schema":
                {"properties": {"cache_control": {"type": ty}}}}],
            "messages": [{"role": "user", "content": "hi"}],
        })
    };
    let h1 = compute_structural_hash(&with_schema("string"), ApiKind::Anthropic);
    let h2 = compute_structural_hash(&with_schema("integer"), ApiKind::Anthropic);
    assert_eq!(drift_dims(&h1, &h2), "tools");

    let with_tool_input = |v: &str| {
        json!({
            "model": "m", "system": "s", "tools": [],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "x", "name": "t",
                     "input": {"cache_control": v}},
                ]},
            ],
        })
    };
    let h3 = compute_structural_hash(&with_tool_input("a"), ApiKind::Anthropic);
    let h4 = compute_structural_hash(&with_tool_input("b"), ApiKind::Anthropic);
    assert_eq!(drift_dims(&h3, &h4), "early_messages");
}

#[test]
fn bare_string_messages_only_count_for_responses_input() {
    // `input: "text"` is valid Responses sugar; a bare-string
    // `messages` on the other shapes is malformed and contributes
    // no conversation identity.
    let responses = json!({"model": "m", "instructions": "i", "input": "hello"});
    assert_eq!(
        conversation_messages(&responses, ApiKind::OpenAiResponses).len(),
        1
    );
    let malformed = json!({"model": "m", "system": "s", "messages": "oops"});
    assert!(conversation_messages(&malformed, ApiKind::Anthropic).is_empty());
    assert!(conversation_discriminator(&malformed, ApiKind::Anthropic, None) == "-");
}

#[test]
fn openai_chat_discriminator_uses_first_non_system_message() {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer shared-workspace-token".parse().unwrap(),
    );
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
    let conv = |first_user: &str| {
        json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "shared assistant config"},
                {"role": "user", "content": first_user},
            ],
        })
    };
    let key_a = derive_session_key(&headers, &addr, &conv("opener A"), ApiKind::OpenAiChat);
    let key_b = derive_session_key(&headers, &addr, &conv("opener B"), ApiKind::OpenAiChat);
    assert_ne!(key_a, key_b);
}

#[test]
fn openai_responses_discriminator_uses_first_input_item() {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer shared-workspace-token".parse().unwrap(),
    );
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
    let conv = |first_input: &str| {
        json!({
            "model": "gpt-4",
            "instructions": "shared instructions",
            "input": [{"type": "message", "role": "user", "content": first_input}],
        })
    };
    let key_a = derive_session_key(&headers, &addr, &conv("opener A"), ApiKind::OpenAiResponses);
    let key_b = derive_session_key(&headers, &addr, &conv("opener B"), ApiKind::OpenAiResponses);
    assert_ne!(key_a, key_b);
}

#[test]
fn early_messages_window_caps_at_three() {
    // 5 messages: hash should depend only on the first 3.
    let h1 = compute_structural_hash(
        &anthropic_body("s", json!([]), vec!["a", "b", "c", "d", "e"]),
        ApiKind::Anthropic,
    );
    // Mutating message 4 only must NOT drift the early_messages hash.
    let h2 = compute_structural_hash(
        &anthropic_body("s", json!([]), vec!["a", "b", "c", "DIFFERENT", "e"]),
        ApiKind::Anthropic,
    );
    assert_eq!(h1.early_messages, h2.early_messages);
    // But mutating message 1 must drift it.
    let h3 = compute_structural_hash(
        &anthropic_body("s", json!([]), vec!["DIFFERENT", "b", "c", "d", "e"]),
        ApiKind::Anthropic,
    );
    assert_ne!(h1.early_messages, h3.early_messages);
}
