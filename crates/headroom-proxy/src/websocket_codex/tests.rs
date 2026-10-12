use super::*;
use serde_json::json;

fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut m = HeaderMap::new();
    for (k, v) in pairs {
        m.append(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    m
}

// ── path routing ─────────────────────────────────────────────

#[test]
fn codex_paths_recognized() {
    for p in [
        "/v1/responses",
        "/v1/codex/responses",
        "/backend-api/responses",
        "/backend-api/codex/responses",
    ] {
        assert!(is_codex_responses_path(p), "{p}");
    }
    assert!(!is_codex_responses_path("/v1/responses/xyz"));
    assert!(!is_codex_responses_path("/v1/chat/completions"));
    assert!(!is_codex_responses_path("/ws"));
}

// ── WS→HTTP fallback read bound ─────────────────────────────

/// `count` items, each arriving `gap` after the previous one.
fn spaced_items(
    count: usize,
    gap: Duration,
) -> impl futures_util::Stream<Item = Result<usize, &'static str>> {
    futures_util::stream::unfold(0usize, move |i| async move {
        if i >= count {
            return None;
        }
        tokio::time::sleep(gap).await;
        Some((Ok(i), i + 1))
    })
}

#[tokio::test(start_paused = true)]
async fn idle_gap_lets_a_steady_stream_outlive_the_bound() {
    // 30 chunks, 10s apart: five minutes of wall clock, no gap
    // anywhere near the 120s bound. This is the Codex turn the
    // old total timeout cut off at two minutes.
    let started = tokio::time::Instant::now();
    let stream = Box::pin(with_idle_gap(
        Box::pin(spaced_items(30, Duration::from_secs(10))),
        WS_HTTP_FALLBACK_READ_TIMEOUT,
    ));
    let items: Vec<_> = stream.collect().await;

    assert_eq!(items.len(), 30);
    assert!(items.iter().all(|item| item.is_ok()), "{items:?}");
    assert!(started.elapsed() >= Duration::from_secs(300));
}

#[tokio::test(start_paused = true)]
async fn idle_gap_errors_on_a_single_long_silence() {
    let stream = Box::pin(with_idle_gap(
        Box::pin(spaced_items(1, Duration::from_secs(200))),
        WS_HTTP_FALLBACK_READ_TIMEOUT,
    ));
    let items: Vec<_> = stream.collect().await;

    assert_eq!(items.len(), 1);
    assert!(matches!(items[0], Err(GapError::Idle)), "{items:?}");
}

#[tokio::test(start_paused = true)]
async fn idle_gap_surfaces_an_inner_error() {
    let inner = futures_util::stream::iter(vec![Ok(1usize), Err("boom")]);
    let stream = Box::pin(with_idle_gap(
        Box::pin(inner),
        WS_HTTP_FALLBACK_READ_TIMEOUT,
    ));
    let items: Vec<_> = stream.collect().await;

    assert_eq!(items.len(), 2);
    assert!(
        matches!(items[1], Err(GapError::Inner("boom"))),
        "{items:?}"
    );
}

// ── origin policy ────────────────────────────────────────────

#[test]
fn origin_missing_is_allowed() {
    assert!(is_allowed_websocket_origin(None, None));
    assert!(is_allowed_websocket_origin(Some(""), None));
}

#[test]
fn origin_loopback_allowed_without_env() {
    assert!(is_allowed_websocket_origin(
        Some("http://localhost:3000"),
        None
    ));
    assert!(is_allowed_websocket_origin(Some("http://127.0.0.1"), None));
    assert!(!is_allowed_websocket_origin(
        Some("https://evil.example.com"),
        None
    ));
}

#[test]
fn origin_env_allowlist_and_wildcard() {
    let allowed = vec!["https://app.example.com".to_string()];
    assert!(is_allowed_websocket_origin(
        Some("https://app.example.com"),
        Some(&allowed)
    ));
    assert!(is_allowed_websocket_origin(
        // default port elided on both sides
        Some("https://app.example.com:443"),
        Some(&allowed)
    ));
    assert!(!is_allowed_websocket_origin(
        Some("https://other.example.com"),
        Some(&allowed)
    ));
    let wild = vec!["*".to_string()];
    assert!(is_allowed_websocket_origin(
        Some("https://anything.example.com"),
        Some(&wild)
    ));
}

#[test]
fn normalize_origin_cases() {
    assert_eq!(
        normalize_origin("HTTPS://App.Example.COM:443").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(
        normalize_origin("http://h:8080").as_deref(),
        Some("http://h:8080")
    );
    assert_eq!(normalize_origin("ftp://x"), None);
    assert_eq!(normalize_origin("not a url"), None);
}

// ── client classification / stamping ─────────────────────────

#[test]
fn classify_client_explicit_header_wins() {
    let h = hm(&[("x-client", " Codex "), ("user-agent", "aider/1.0")]);
    assert_eq!(classify_client(&h).as_deref(), Some("codex"));
}

#[test]
fn classify_client_ua_substring() {
    let h = hm(&[("user-agent", "corp-wrapper codex-cli/2.1")]);
    assert_eq!(classify_client(&h).as_deref(), Some("codex"));
    let h = hm(&[("user-agent", "Mozilla/5.0")]);
    assert_eq!(classify_client(&h), None);
    let h = hm(&[("user-agent", "grok-shell/0.2.112")]);
    assert_eq!(classify_client(&h).as_deref(), Some("grok_build"));
}

#[test]
fn stamp_codex_only_for_unidentified_on_responses_path() {
    let unknown = hm(&[("user-agent", "Codex Desktop/1.0")]);
    assert!(should_stamp_codex_client("/v1/responses", &unknown));
    assert!(should_stamp_codex_client("/v1/responses/sub", &unknown));
    assert!(!should_stamp_codex_client(
        "/backend-api/responses",
        &unknown
    ));
    let known = hm(&[("user-agent", "codex-cli/1.0")]);
    assert!(!should_stamp_codex_client("/v1/responses", &known));
}

#[test]
fn stamp_codex_covers_pi_responses_alias() {
    // Pi-compatible callers (e.g. Gajae-Code) open the Responses
    // WebSocket at `/v1/codex/responses` with an unrecognized UA.
    let pi = hm(&[("user-agent", "pi/0.11.11 (darwin 25.5.0; arm64)")]);
    assert!(should_stamp_codex_client("/v1/codex/responses", &pi));
    assert!(should_stamp_codex_client(
        "/v1/codex/responses/compact",
        &pi
    ));
    assert!(!should_stamp_codex_client("/v1/codex/responses-other", &pi));
    assert!(!should_stamp_codex_client("/v1/codex", &pi));
    let known = hm(&[("user-agent", "codex-cli/1.0")]);
    assert!(!should_stamp_codex_client("/v1/codex/responses", &known));
}

// ── merge_openai_beta ────────────────────────────────────────

#[test]
fn openai_beta_merge_appends_required_token() {
    let merged =
        crate::headers::merge_beta_tokens(Some("foo=1,Bar=2"), &[RESPONSES_WS_REQUIRED_BETA]);
    assert_eq!(merged, format!("foo=1,Bar=2,{RESPONSES_WS_REQUIRED_BETA}"));
}

#[test]
fn openai_beta_merge_dedupes_case_insensitively() {
    let upper = RESPONSES_WS_REQUIRED_BETA.to_ascii_uppercase();
    let merged =
        crate::headers::merge_beta_tokens(Some(upper.as_str()), &[RESPONSES_WS_REQUIRED_BETA]);
    // First occurrence's casing wins; no duplicate appended.
    assert_eq!(merged, upper);
}

// ── JWT routing hint ─────────────────────────────────────────

fn make_jwt(payload: &Value) -> String {
    let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    format!(
        "{}.{}.{}",
        enc(b"{\"alg\":\"none\"}"),
        enc(payload.to_string().as_bytes()),
        enc(b"sig")
    )
}

#[test]
fn jwt_payload_decodes_without_verification() {
    let token = make_jwt(&json!({"sub": "u1"}));
    let payload = decode_bearer_jwt_payload(&format!("Bearer {token}")).unwrap();
    assert_eq!(payload["sub"], "u1");
}

#[test]
fn jwt_decode_rejects_non_bearer_and_non_jwt() {
    assert!(decode_bearer_jwt_payload("Basic dXNlcjpwYXNz").is_none());
    assert!(decode_bearer_jwt_payload("Bearer sk-not-a-jwt").is_none());
    assert!(decode_bearer_jwt_payload("Bearer a.b").is_none());
    assert!(decode_bearer_jwt_payload("Bearer a.!!!.c").is_none());
}

#[test]
fn routing_explicit_header_short_circuits() {
    let mut h = hm(&[("chatgpt-account-id", "acct_1")]);
    assert!(resolve_codex_routing(&mut h));
}

#[test]
fn routing_derives_account_id_from_jwt() {
    let token = make_jwt(&json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": " acct_42 "}
    }));
    let mut h = hm(&[("authorization", &format!("Bearer {token}"))]);
    assert!(resolve_codex_routing(&mut h));
    assert_eq!(
        h.get("chatgpt-account-id").unwrap().to_str().unwrap(),
        "acct_42"
    );
}

#[test]
fn routing_no_hint_is_api_route() {
    let mut h = hm(&[("authorization", "Bearer sk-test-123")]);
    assert!(!resolve_codex_routing(&mut h));
    assert!(!h.contains_key("chatgpt-account-id"));
}

// ── usage extraction ─────────────────────────────────────────

#[test]
fn usage_only_from_response_completed() {
    let ev = json!({"type": "response.output_text.delta", "usage": {"input_tokens": 5}});
    assert_eq!(extract_responses_usage(&ev), ResponsesUsage::default());
}

#[test]
fn usage_extracted_with_cache_details() {
    let ev = json!({
        "type": "response.completed",
        "response": {
            "id": "resp_1",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 25,
                "input_tokens_details": {"cached_tokens": 60}
            }
        }
    });
    let u = extract_responses_usage(&ev);
    assert_eq!(u.input_tokens, 100);
    assert_eq!(u.output_tokens, 25);
    assert_eq!(u.cache_read_tokens, 60);
    // FINDING-025: inferred write leg is 0 (billed once via uncached).
    assert_eq!(u.cache_write_tokens, 0);
    assert_eq!(u.uncached_tokens, 40);
}

#[test]
fn usage_tolerates_top_level_usage_and_garbage() {
    let ev =
        json!({"type": "response.completed", "usage": {"input_tokens": 7, "output_tokens": 3}});
    let u = extract_responses_usage(&ev);
    assert_eq!(u.input_tokens, 7);
    assert_eq!(u.output_tokens, 3);
    let ev = json!({"type": "response.completed"});
    assert_eq!(extract_responses_usage(&ev), ResponsesUsage::default());
    let ev = json!({"type": "response.completed", "response": {"usage": {"input_tokens": "x"}}});
    assert_eq!(extract_responses_usage(&ev).input_tokens, 0);
}

// ── frame compression envelope logic ─────────────────────────

const RID: &str = "req-test";

#[test]
fn frame_non_json_passthrough() {
    let out = compress_response_create_frame(
        "not json",
        CompressionMode::LiveZone,
        AuthMode::Payg,
        RID,
        &[],
        &Default::default(),
    );
    assert!(matches!(
        out,
        FrameCompression::Passthrough { reason: "non_json" }
    ));
}

#[test]
fn frame_not_response_create_passthrough() {
    let raw = json!({"type": "session.update", "x": 1}).to_string();
    let out = compress_response_create_frame(
        &raw,
        CompressionMode::LiveZone,
        AuthMode::Payg,
        RID,
        &[],
        &Default::default(),
    );
    assert!(matches!(
        out,
        FrameCompression::Passthrough {
            reason: "not_response_create"
        }
    ));
}

/// The live frame path reads `--phase-e-responses-passes` and folds
/// `--auth-mode-policy-enforcement` like the HTTP paths: with
/// enforcement off an OAuth caller gets the E1 tool sort, and with E1
/// unlisted nobody does.
#[tokio::test]
async fn bounded_frame_honours_phase_e_policy_and_enforcement() {
    let raw = json!({
        "type": "response.create",
        "model": "gpt-4o",
        "input": [{"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "hi"}]}],
        "tools": [
            {"type": "function", "function": {"name": "zebra"}},
            {"type": "function", "function": {"name": "apple"}},
        ],
    })
    .to_string();
    let mut config = crate::config::Config::for_test("http://127.0.0.1:9".parse().unwrap());
    config.auth_mode_policy_enforcement = crate::config::AuthModePolicyEnforcement::Disabled;
    let sorted = |a: CompressAttempt| {
        matches!(a, CompressAttempt::Done(FrameCompression::Compressed { strategies, .. })
                if strategies.contains(&"tool_array_sort"))
    };
    let state = AppState::new(config.clone()).unwrap();
    let out = compress_frame_bounded(
        raw.clone(),
        CompressionMode::LiveZone,
        AuthMode::OAuth,
        RID.into(),
        &state,
    );
    assert!(sorted(out.await), "enforcement off: OAuth counts as PAYG");

    config
        .phase_e_passes
        .responses
        .remove(&crate::config::PhaseEPass::E1);
    let state = AppState::new(config).unwrap();
    let out = compress_frame_bounded(
        raw,
        CompressionMode::LiveZone,
        AuthMode::Payg,
        RID.into(),
        &state,
    );
    assert!(!sorted(out.await), "E1 unlisted: no tool sort");
}

#[test]
fn frame_invalid_inner_payload_passthrough() {
    let raw = json!({"type": "response.create", "response": "nope"}).to_string();
    let out = compress_response_create_frame(
        &raw,
        CompressionMode::LiveZone,
        AuthMode::Payg,
        RID,
        &[],
        &Default::default(),
    );
    assert!(matches!(
        out,
        FrameCompression::Passthrough {
            reason: "invalid_inner_payload"
        }
    ));
}

#[test]
fn frame_mode_off_passthrough() {
    let raw = json!({
            "type": "response.create",
            "response": {"model": "gpt-x", "input": [{"type": "message", "role": "user", "content": "hi"}]}
        })
        .to_string();
    let out = compress_response_create_frame(
        &raw,
        CompressionMode::Off,
        AuthMode::Payg,
        RID,
        &[],
        &Default::default(),
    );
    assert!(matches!(
        out,
        FrameCompression::Passthrough {
            reason: "optimize_disabled"
        }
    ));
}

fn big_compressible_inner() -> Value {
    // Repetitive structured log output — reliably compressible by the
    // live-zone content router at >2 KiB.
    let blob =
        "{\"level\": \"info\", \"msg\": \"request handled ok\", \"latency_ms\": 12}\n".repeat(400);
    json!({
        "model": "gpt-5.4-codex",
        "instructions": "You are Codex.",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "run tests"}]},
            {"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "call_1"},
            {"type": "function_call_output", "call_id": "call_1", "output": blob}
        ],
        "stream": true
    })
}

#[test]
fn frame_wrapped_envelope_compresses_and_rewraps() {
    let raw = json!({"type": "response.create", "response": big_compressible_inner()}).to_string();
    let out = compress_response_create_frame(
        &raw,
        CompressionMode::LiveZone,
        AuthMode::Payg,
        RID,
        &[],
        &Default::default(),
    );
    match out {
        FrameCompression::Compressed {
            text,
            tokens_before,
            tokens_after,
            bytes_before,
            bytes_after,
            ..
        } => {
            assert!(tokens_after < tokens_before);
            let parsed: Value = serde_json::from_str(&text).unwrap();
            // Envelope shape preserved.
            assert_eq!(parsed["type"], "response.create");
            assert!(parsed["response"].is_object());
            assert_eq!(parsed["response"]["model"], "gpt-5.4-codex");
            assert!(text.len() < raw.len());
            // FINDING-026: byte legs are inner-payload spans, not the
            // outer envelope — the compressed output's inner payload
            // must equal bytes_after, and both legs sit inside raw/text.
            let inner_len = serde_json::to_vec(&parsed["response"]).unwrap().len();
            assert!(bytes_before < raw.len());
            assert_eq!(bytes_after, inner_len);
            assert!(bytes_after < text.len());
        }
        other => panic!("expected Compressed, got {other:?}"),
    }
}

#[test]
fn frame_additional_tools_are_restored_for_stateful_sessions() {
    let mut inner = big_compressible_inner();
    inner["input"].as_array_mut().unwrap().insert(
        1,
        json!({
            "type": "additional_tools",
            "id": "tool-transcript-item",
            "tools": [{
                "type": "function",
                "name": "shell",
                "description": "Run a shell command",
                "parameters": {"type": "object", "properties": {}}
            }]
        }),
    );
    let raw = json!({"type": "response.create", "response": inner}).to_string();
    let out = compress_response_create_frame(
        &raw,
        CompressionMode::LiveZone,
        AuthMode::Payg,
        RID,
        &[],
        &Default::default(),
    );
    let FrameCompression::Compressed { text, .. } = out else {
        panic!("expected compressed frame, got {out:?}");
    };
    let forwarded: Value = serde_json::from_str(&text).unwrap();
    let response = &forwarded["response"];
    assert!(response.get("tools").is_none());
    let carrier = response["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "additional_tools")
        .expect("tool carrier restored into the transcript");
    assert_eq!(carrier["id"], "tool-transcript-item");
    assert_eq!(carrier["tools"][0]["name"], "shell");
}

#[test]
fn frame_bare_envelope_compresses_in_place() {
    let mut bare = big_compressible_inner();
    bare.as_object_mut()
        .unwrap()
        .insert("type".to_string(), json!("response.create"));
    let raw = bare.to_string();
    let out = compress_response_create_frame(
        &raw,
        CompressionMode::LiveZone,
        AuthMode::Payg,
        RID,
        &[],
        &Default::default(),
    );
    match out {
        FrameCompression::Compressed { text, .. } => {
            let parsed: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(parsed["type"], "response.create");
            assert!(parsed.get("response").is_none());
            assert!(text.len() < raw.len());
        }
        other => panic!("expected Compressed, got {other:?}"),
    }
}

// ── termination classification ───────────────────────────────

#[test]
fn termination_matrix() {
    use TerminationCause::*;
    assert_eq!(
        classify_termination(FirstDone::Client { error: true }, false, 0),
        ClientError
    );
    assert_eq!(
        classify_termination(FirstDone::Client { error: false }, false, 0),
        ClientDisconnect
    );
    assert_eq!(
        classify_termination(FirstDone::Upstream { error: true }, false, 0),
        UpstreamError
    );
    assert_eq!(
        classify_termination(FirstDone::Upstream { error: false }, true, 0),
        ResponseCompleted
    );
    assert_eq!(
        classify_termination(FirstDone::Upstream { error: false }, false, 0),
        UpstreamDisconnect
    );
}

#[test]
fn termination_client_cancel_override() {
    use TerminationCause::*;
    // Cancel frames + no completed response + disconnect-ish cause.
    assert_eq!(
        classify_termination(FirstDone::Upstream { error: false }, false, 1),
        ClientCancel
    );
    assert_eq!(
        classify_termination(FirstDone::Client { error: false }, false, 2),
        ClientCancel
    );
    // Completed response suppresses the override.
    assert_eq!(
        classify_termination(FirstDone::Upstream { error: false }, true, 1),
        ResponseCompleted
    );
    // Error causes are not overridden.
    assert_eq!(
        classify_termination(FirstDone::Upstream { error: true }, false, 1),
        UpstreamError
    );
}
