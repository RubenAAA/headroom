use super::*;
use serde_json::json;

fn body(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

fn outcome_block_actions(o: &LiveZoneOutcome) -> Vec<&BlockAction> {
    let manifest = match o {
        LiveZoneOutcome::NoChange { manifest } => manifest,
        LiveZoneOutcome::Modified { manifest, .. } => manifest,
    };
    manifest.block_outcomes.iter().map(|b| &b.action).collect()
}

#[test]
fn empty_messages_yields_no_change() {
    let b = body(json!({"model": "claude", "messages": []}));
    let out = compress_anthropic_live_zone(&b, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    match out {
        LiveZoneOutcome::NoChange { manifest } => {
            assert_eq!(manifest.messages_total, 0);
            assert_eq!(manifest.latest_user_message_index, None);
            assert!(manifest.block_outcomes.is_empty());
        }
        _ => panic!("expected NoChange"),
    }
}

#[test]
fn no_messages_field_errors() {
    let b = body(json!({"model": "claude"}));
    let err = compress_anthropic_live_zone(&b, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap_err();
    assert!(matches!(err, LiveZoneError::NoMessagesArray));
}

#[test]
fn invalid_json_errors() {
    let err =
        compress_anthropic_live_zone(b"not json", 0, AuthMode::Payg, DEFAULT_MODEL).unwrap_err();
    assert!(matches!(err, LiveZoneError::BodyNotJson(_)));
}

#[test]
fn dispatches_only_to_latest_user_message() {
    // Two user messages; the dispatcher must pick the second (index 2).
    let b = body(json!({
        "messages": [
            {"role": "user", "content": "first user"},
            {"role": "assistant", "content": "first asst"},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "result"},
                {"type": "text", "text": "summarize"}
            ]},
        ]
    }));
    let out = compress_anthropic_live_zone(&b, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    let manifest = match &out {
        LiveZoneOutcome::NoChange { manifest } => manifest,
        LiveZoneOutcome::Modified { manifest, .. } => manifest,
    };
    assert_eq!(manifest.latest_user_message_index, Some(2));
    let block_msg_indices: Vec<usize> = manifest
        .block_outcomes
        .iter()
        .map(|b| b.message_index)
        .collect();
    assert!(
        block_msg_indices.iter().all(|i| *i == 2),
        "all block outcomes must reference the latest user message; got {block_msg_indices:?}"
    );
}

#[test]
fn respects_frozen_message_count() {
    // Latest user message is at index 1; floor is 2 → live zone is empty.
    let b = body(json!({
        "messages": [
            {"role": "user", "content": "first"},
            {"role": "user", "content": [{"type": "text", "text": "second"}]},
        ]
    }));
    let out = compress_anthropic_live_zone(&b, 2, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    let manifest = match &out {
        LiveZoneOutcome::NoChange { manifest } => manifest,
        _ => panic!("expected NoChange"),
    };
    assert_eq!(manifest.latest_user_message_index, None);
    assert!(manifest.block_outcomes.is_empty());
    assert_eq!(manifest.messages_below_frozen_floor, 2);
}

#[test]
fn excludes_hot_zone_block_types() {
    let b = body(json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "x"},
                {"type": "thinking", "thinking": "...", "signature": "sig"},
                {"type": "text", "text": "ok"},
            ]
        }]
    }));
    let out = compress_anthropic_live_zone(&b, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    let actions = outcome_block_actions(&out);
    assert_eq!(actions.len(), 3);
    // tool_result with tiny content → BelowByteThreshold.
    assert!(matches!(actions[0], BlockAction::BelowByteThreshold { .. }));
    assert!(matches!(
        actions[1],
        BlockAction::Excluded {
            reason: ExclusionReason::HotZoneBlockType
        }
    ));
    // text block with "ok" → BelowByteThreshold.
    assert!(matches!(actions[2], BlockAction::BelowByteThreshold { .. }));
}

#[test]
fn string_content_message_records_synthetic_block() {
    let b = body(json!({
        "messages": [{"role": "user", "content": "just a string"}]
    }));
    let out = compress_anthropic_live_zone(&b, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    let manifest = match &out {
        LiveZoneOutcome::NoChange { manifest } => manifest,
        LiveZoneOutcome::Modified { manifest, .. } => manifest,
    };
    assert_eq!(manifest.block_outcomes.len(), 1);
    assert_eq!(manifest.block_outcomes[0].block_type, "string_content");
    // 13 bytes of plain text is well below the plain-text threshold.
    assert!(matches!(
        manifest.block_outcomes[0].action,
        BlockAction::BelowByteThreshold { .. }
    ));
}

#[test]
fn no_user_message_in_live_zone_returns_no_blocks() {
    let b = body(json!({
        "messages": [{"role": "assistant", "content": "hi"}]
    }));
    let out = compress_anthropic_live_zone(&b, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    let manifest = match &out {
        LiveZoneOutcome::NoChange { manifest } => manifest,
        _ => panic!("expected NoChange"),
    };
    assert_eq!(manifest.latest_user_message_index, None);
    assert!(manifest.block_outcomes.is_empty());
}

#[test]
fn auth_mode_does_not_affect_b3_outcome_for_short_input() {
    // Trivial input → every mode behaves identically.
    let b = body(json!({
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    }));
    let payg = compress_anthropic_live_zone(&b, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    let oauth = compress_anthropic_live_zone(&b, 0, AuthMode::OAuth, DEFAULT_MODEL).unwrap();
    let sub = compress_anthropic_live_zone(&b, 0, AuthMode::Subscription, DEFAULT_MODEL).unwrap();
    for o in [&payg, &oauth, &sub] {
        assert!(matches!(o, LiveZoneOutcome::NoChange { .. }));
    }
}

#[test]
fn no_change_when_input_already_minimal_returns_original_semantics() {
    // tiny tool_result → detected as plain text, no-op
    // dispatch → NoChange.
    let b = body(json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "x"},
            ]
        }]
    }));
    let out = compress_anthropic_live_zone(&b, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    assert!(matches!(out, LiveZoneOutcome::NoChange { .. }));
}

#[test]
fn block_has_string_text_field_detects_converse_text_only() {
    // Converse text block: typeless, string `text` → recognized.
    assert!(block_has_string_text_field(r#"{"text":"hello"}"#));
    // Non-text Converse blocks must NOT be mistaken for text.
    assert!(!block_has_string_text_field(
        r#"{"image":{"format":"png"}}"#
    ));
    assert!(!block_has_string_text_field(r#"{"toolUse":{"name":"x"}}"#));
    // `text` present but not a JSON string → not Converse text.
    assert!(!block_has_string_text_field(r#"{"text":["a"]}"#));
    assert!(!block_has_string_text_field(r#"{"text":{"v":1}}"#));
}

#[test]
fn converse_typeless_text_block_routes_like_anthropic_text() {
    // Bedrock Converse content blocks omit the `type` discriminator —
    // `{"text": "..."}` instead of `{"type":"text","text":"..."}`. The
    // dispatcher must treat the two identically so Converse user-message
    // text compresses like Anthropic text.
    let payload = "{\"k\": \"v\", \"n\": 1}\n".repeat(200);
    let converse = body(json!({
        "messages": [{"role": "user", "content": [{"text": payload}]}]
    }));
    let anthropic = body(json!({
        "messages": [{"role": "user", "content": [{"type": "text", "text": payload}]}]
    }));
    let c = compress_anthropic_live_zone(&converse, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    let a = compress_anthropic_live_zone(&anthropic, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();

    // Identical dispatch outcome (both Modified or both NoChange).
    assert_eq!(
        std::mem::discriminant(&c),
        std::mem::discriminant(&a),
        "converse text block must dispatch like an anthropic text block"
    );
    let cm = match &c {
        LiveZoneOutcome::NoChange { manifest } => manifest,
        LiveZoneOutcome::Modified { manifest, .. } => manifest,
    };
    let am = match &a {
        LiveZoneOutcome::NoChange { manifest } => manifest,
        LiveZoneOutcome::Modified { manifest, .. } => manifest,
    };
    // The Converse block is now classified the same as Anthropic text
    // (before this change it was an unrecognized typeless block).
    assert_eq!(cm.block_outcomes.len(), 1);
    assert_eq!(am.block_outcomes.len(), 1);
    assert_eq!(
        cm.block_outcomes[0].block_type,
        am.block_outcomes[0].block_type
    );
    assert_eq!(cm.block_outcomes[0].block_type, "text");
}

/// A payload the dispatcher reliably compresses, so a test that
/// asserts "not compressed" is actually testing the guard.
fn compressible_payload() -> String {
    "{\"k\": \"v\", \"n\": 1}\n".repeat(200)
}

/// Negative control for the two CCR-retrieve guard tests below: the
/// same payload under an ordinary tool must still compress, so the
/// guard is not just "tool_results are never touched".
#[test]
fn normal_tool_result_still_compresses() {
    let b = body(json!({
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": compressible_payload()}
            ]},
        ]
    }));
    let out = compress_anthropic_live_zone(&b, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    assert!(
        matches!(
            outcome_block_actions(&out).as_slice(),
            [BlockAction::Compressed { .. }]
        ),
        "an ordinary tool_result of this payload must compress; got {:?}",
        outcome_block_actions(&out)
    );
}

/// The bug: recompressing a headroom_retrieve result writes a new
/// `<<ccr:hash>>` marker the agent can never redeem.
#[test]
fn ccr_retrieve_tool_result_is_never_compressed() {
    for name in [
        "headroom_retrieve",
        "mcp__Headroom__headroom_retrieve",
        "mcp_Headroom_headroom_retrieve",
    ] {
        let b = body(json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "ccr1", "name": name, "input": {"hash": "abc"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "ccr1",
                     "content": compressible_payload()}
                ]},
            ]
        }));
        let out = compress_anthropic_live_zone(&b, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();
        assert!(
            matches!(out, LiveZoneOutcome::NoChange { .. }),
            "{name}: body must be forwarded unmodified"
        );
        assert!(
            matches!(
                outcome_block_actions(&out).as_slice(),
                [BlockAction::Excluded {
                    reason: ExclusionReason::CcrRetrieveResult
                }]
            ),
            "{name}: got {:?}",
            outcome_block_actions(&out)
        );
    }
}

/// The decay case: the all-messages dispatcher reaches tool_results
/// that have aged out of the latest user message. An aged CCR marker
/// is exactly as unredeemable as a fresh one, so the guard must hold
/// there too — while the ordinary tool_result beside it still
/// compresses.
#[test]
fn aged_out_ccr_retrieve_tool_result_is_never_compressed() {
    let b = body(json!({
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "ccr1", "name": "headroom_retrieve",
                 "input": {"hash": "abc"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "ccr1",
                 "content": compressible_payload()}
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "b1", "name": "Bash", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "b1",
                 "content": compressible_payload()}
            ]},
        ]
    }));
    let out = compress_anthropic_all_messages(
        &b,
        AuthMode::Payg,
        DEFAULT_MODEL,
        None,
        &DispatchConfig::default(),
    )
    .unwrap();
    let manifest = match &out {
        LiveZoneOutcome::NoChange { manifest } => manifest,
        LiveZoneOutcome::Modified { manifest, .. } => manifest,
    };
    let by_msg = |idx: usize| {
        manifest
            .block_outcomes
            .iter()
            .find(|b| b.message_index == idx)
            .map(|b| &b.action)
    };
    assert!(
        matches!(
            by_msg(1),
            Some(BlockAction::Excluded {
                reason: ExclusionReason::CcrRetrieveResult
            })
        ),
        "aged-out retrieve result must stay excluded; got {:?}",
        by_msg(1)
    );
    assert!(
        matches!(by_msg(3), Some(BlockAction::Compressed { .. })),
        "the ordinary aged-out tool_result must still compress; got {:?}",
        by_msg(3)
    );
}

/// ctx-offload runs before this pass and leaves a digest carrying
/// `<<ctx:hash>>`. Compressing that digest again would append a second
/// `<<ccr:hash>>` marker redeeming to the digest instead of the original,
/// burying the pointer to the true bytes under a lossy copy.
#[test]
fn a_ctx_offload_digest_is_not_compressed_again() {
    let digest = format!(
        "{}\n<<ctx:deadbeef>> (60000 bytes offloaded; \
             use the headroom_retrieve tool with hash=\"deadbeef\")",
        compressible_payload()
    );
    let b = body(json!({
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": digest}
            ]},
        ]
    }));
    let out = compress_anthropic_all_messages(
        &b,
        AuthMode::Payg,
        DEFAULT_MODEL,
        None,
        &DispatchConfig::default(),
    )
    .unwrap();
    let manifest = match &out {
        LiveZoneOutcome::NoChange { manifest } => manifest,
        LiveZoneOutcome::Modified { manifest, .. } => manifest,
    };
    let action = manifest
        .block_outcomes
        .iter()
        .find(|b| b.message_index == 1)
        .map(|b| &b.action);
    assert!(
        matches!(
            action,
            Some(BlockAction::Excluded {
                reason: ExclusionReason::CtxOffloadDigest
            })
        ),
        "an offload digest must be left alone; got {action:?}"
    );
}

// ─── `--exclude-tools` ────────────────────────────────────────────

/// One `tool_use` + one `tool_result` carrying `payload`.
fn tool_result_body(tool_name: &str, payload: &str) -> Vec<u8> {
    body(json!({
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": tool_name, "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": payload}
            ]},
        ]
    }))
}

fn run_excluding(b: &[u8], exclude: &[&str]) -> LiveZoneOutcome {
    let config = DispatchConfig {
        exclude_tools: exclude.iter().map(|s| s.to_string()).collect(),
        ..DispatchConfig::default()
    };
    compress_anthropic_live_zone_with_ccr(b, 0, AuthMode::Payg, DEFAULT_MODEL, None, &config)
        .unwrap()
}

fn outcome_bytes(o: &LiveZoneOutcome) -> Option<String> {
    match o {
        LiveZoneOutcome::NoChange { .. } => None,
        LiveZoneOutcome::Modified { new_body, .. } => Some(new_body.get().to_string()),
    }
}

/// The rewritten `tool_result` content of a Modified outcome.
fn rewritten_tool_result(o: &LiveZoneOutcome) -> String {
    let raw = outcome_bytes(o).expect("outcome must be Modified");
    let v: Value = serde_json::from_str(&raw).unwrap();
    v["messages"][1]["content"][0]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A build log whose repeated lines `collapse_runs` folds exactly.
fn repeated_log_payload() -> String {
    format!(
        "2024-01-01 12:00:00 INFO  worker: starting up\n{}",
        "2024-01-01 12:00:01 WARN  worker: retrying connection\n".repeat(200)
    )
}

/// The non-negotiable default: with no `--exclude-tools`, every byte
/// the dispatcher writes must match what it wrote before the flag was
/// wired. Tools listed in Python's `DEFAULT_EXCLUDE_TOOLS` are the
/// tripwire — if that set ever leaks in as the Rust default, `Read`
/// stops compressing and this fails.
#[test]
fn empty_exclude_list_is_byte_identical_to_the_default_path() {
    assert!(DispatchConfig::default().exclude_tools.is_empty());
    for tool in ["Read", "Grep", "WebFetch", "Bash"] {
        let b = tool_result_body(tool, &compressible_payload());
        let baseline = compress_anthropic_live_zone(&b, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();
        let with_empty_list = run_excluding(&b, &[]);
        assert_eq!(
            outcome_bytes(&baseline),
            outcome_bytes(&with_empty_list),
            "{tool}: empty exclusion list changed the output bytes"
        );
        assert!(
            matches!(
                outcome_block_actions(&with_empty_list).as_slice(),
                [BlockAction::Compressed { .. }]
            ),
            "{tool}: must still compress by default; got {:?}",
            outcome_block_actions(&with_empty_list)
        );
    }
}

/// An excluded tool's result must not reach a lossy compressor. This
/// payload is a JSON array, which no reversible fold covers, so the
/// block is forwarded whole rather than degraded.
#[test]
fn excluded_tool_result_is_not_lossily_compressed() {
    let b = tool_result_body("Bash", &compressible_payload());
    let out = run_excluding(&b, &["Bash"]);
    assert!(
        matches!(out, LiveZoneOutcome::NoChange { .. }),
        "excluded tool_result must not rewrite the body"
    );
    assert!(
        matches!(
            outcome_block_actions(&out).as_slice(),
            [BlockAction::Excluded {
                reason: ExclusionReason::ExcludedTool
            }]
        ),
        "got {:?}",
        outcome_block_actions(&out)
    );
}

/// Excluded does not mean untouched: a shape a reversible fold covers
/// still gets folded, and the fold round-trips exactly.
#[test]
fn excluded_tool_result_is_losslessly_compacted_and_round_trips() {
    let payload = repeated_log_payload();
    let b = tool_result_body("Bash", &payload);

    // Without the exclusion the same block goes to the lossy log
    // compressor — so this test measures a swap, not a no-op.
    let lossy = compress_anthropic_live_zone(&b, 0, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    assert!(
        matches!(
            outcome_block_actions(&lossy).as_slice(),
            [BlockAction::Compressed {
                strategy: STRATEGY_LOG_COMPRESSOR,
                ..
            }]
        ),
        "control: got {:?}",
        outcome_block_actions(&lossy)
    );

    let out = run_excluding(&b, &["Bash"]);
    assert!(
        matches!(
            outcome_block_actions(&out).as_slice(),
            [BlockAction::Compressed {
                strategy: STRATEGY_EXCLUDED_TOOL_LOSSLESS,
                ..
            }]
        ),
        "got {:?}",
        outcome_block_actions(&out)
    );
    let compacted = rewritten_tool_result(&out);
    assert!(compacted.len() < payload.len(), "fold must shrink");
    assert_eq!(
        super::super::lossless_compaction::expand_runs(&compacted),
        payload,
        "the fold must invert exactly"
    );
}

/// Search output takes the `search` fold rather than the `log` one.
#[test]
fn excluded_tool_result_picks_the_fold_matching_its_shape() {
    let payload: String = (0..200)
        .map(|i| format!("src/lib.rs:{i}: fn thing_{i}() {{}}\n"))
        .collect();
    let b = tool_result_body("Bash", &payload);
    let out = run_excluding(&b, &["Bash"]);
    assert!(
        matches!(
            outcome_block_actions(&out).as_slice(),
            [BlockAction::Compressed {
                strategy: STRATEGY_EXCLUDED_TOOL_LOSSLESS,
                ..
            }]
        ),
        "got {:?}",
        outcome_block_actions(&out)
    );
    assert_eq!(
        super::super::lossless_compaction::search_unheading(&rewritten_tool_result(&out)),
        payload,
        "the search fold must invert exactly"
    );
}

/// Excluding one tool must not disarm the compressor for the rest.
#[test]
fn non_excluded_tool_result_still_compresses() {
    let b = tool_result_body("Bash", &compressible_payload());
    let out = run_excluding(&b, &["Read", "Grep"]);
    assert!(
        matches!(
            outcome_block_actions(&out).as_slice(),
            [BlockAction::Compressed { .. }]
        ),
        "got {:?}",
        outcome_block_actions(&out)
    );
}

/// Glob and MCP spellings must reach through from the flag to the
/// planner — `tool_exclusion` resolves them, this pins the wiring.
#[test]
fn glob_and_mcp_alias_forms_reach_the_planner() {
    let cases: [(&str, &[&str]); 4] = [
        ("mcp__github__create_issue", &["mcp__*"]),
        ("mcp__github__create_issue", &["mcp__github__create_issue"]),
        ("mcp__github__create_issue", &["mcp_github_create_issue"]),
        ("Bash", &["Ba*h"]),
    ];
    for (tool, patterns) in cases {
        let b = tool_result_body(tool, &compressible_payload());
        let out = run_excluding(&b, patterns);
        assert!(
            matches!(
                outcome_block_actions(&out).as_slice(),
                [BlockAction::Excluded {
                    reason: ExclusionReason::ExcludedTool
                }]
            ),
            "{tool} vs {patterns:?}: got {:?}",
            outcome_block_actions(&out)
        );
    }
}

/// A tool in the verbatim set must not even be folded reversibly —
/// its output breaks on any rewrite.
#[test]
fn verbatim_excluded_tool_result_is_not_even_folded() {
    let b = tool_result_body("WebFetch", &repeated_log_payload());
    let out = run_excluding(&b, &["WebFetch"]);
    assert!(
        matches!(out, LiveZoneOutcome::NoChange { .. }),
        "verbatim-excluded tool_result must not rewrite the body"
    );
    assert!(
        matches!(
            outcome_block_actions(&out).as_slice(),
            [BlockAction::Excluded {
                reason: ExclusionReason::ExcludedTool
            }]
        ),
        "got {:?}",
        outcome_block_actions(&out)
    );
}

/// File-read tools skip the excluded-tool fold as a whole: the model
/// copies `Edit(old_string=…)` anchors from the bytes it is shown, so
/// even a reversible rewrite breaks the next edit.
#[test]
fn byte_exact_file_read_skips_the_lossless_fold() {
    let payload = repeated_log_payload();
    for tool in ["Read", "read", "read_file"] {
        let b = tool_result_body(tool, &payload);
        let out = run_excluding(&b, &[tool]);
        assert!(
            matches!(out, LiveZoneOutcome::NoChange { .. }),
            "{tool}: file-read output must pass through byte-exact"
        );
        assert!(
            matches!(
                outcome_block_actions(&out).as_slice(),
                [BlockAction::Excluded {
                    reason: ExclusionReason::ExcludedTool
                }]
            ),
            "{tool}: got {:?}",
            outcome_block_actions(&out)
        );
    }
    // Control: the same payload under a non-read tool folds.
    let b = tool_result_body("Grep", &payload);
    let out = run_excluding(&b, &["Grep"]);
    assert!(
        matches!(
            outcome_block_actions(&out).as_slice(),
            [BlockAction::Compressed {
                strategy: STRATEGY_EXCLUDED_TOOL_LOSSLESS,
                ..
            }]
        ),
        "control: got {:?}",
        outcome_block_actions(&out)
    );
}

/// Skill bodies are directives, not data: the fold that collapses
/// repeats can merge two distinct instructions into one.
#[test]
fn byte_exact_skill_skips_the_lossless_fold() {
    let b = tool_result_body("Skill", &repeated_log_payload());
    let out = run_excluding(&b, &["Skill"]);
    assert!(
        matches!(out, LiveZoneOutcome::NoChange { .. }),
        "Skill output must pass through byte-exact"
    );
    assert!(
        matches!(
            outcome_block_actions(&out).as_slice(),
            [BlockAction::Excluded {
                reason: ExclusionReason::ExcludedTool
            }]
        ),
        "got {:?}",
        outcome_block_actions(&out)
    );
}

/// The CCR guard is about unredeemable markers, not fidelity, so it
/// outranks the operator's list: a retrieve result stays fully
/// excluded even when a pattern would have routed it to the fold.
#[test]
fn ccr_guard_outranks_the_exclusion_list() {
    let b = tool_result_body("mcp__Headroom__headroom_retrieve", &repeated_log_payload());
    let out = run_excluding(&b, &["mcp__*"]);
    assert!(
        matches!(out, LiveZoneOutcome::NoChange { .. }),
        "retrieve result must be forwarded unmodified"
    );
    assert!(
        matches!(
            outcome_block_actions(&out).as_slice(),
            [BlockAction::Excluded {
                reason: ExclusionReason::CcrRetrieveResult
            }]
        ),
        "got {:?}",
        outcome_block_actions(&out)
    );
}

#[test]
fn manifest_records_messages_below_floor() {
    let b = body(json!({
        "messages": [
            {"role": "user", "content": "frozen"},
            {"role": "assistant", "content": "frozen"},
            {"role": "user", "content": "live"},
        ]
    }));
    let out = compress_anthropic_live_zone(&b, 2, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    let manifest = match &out {
        LiveZoneOutcome::NoChange { manifest } => manifest,
        LiveZoneOutcome::Modified { manifest, .. } => manifest,
    };
    assert_eq!(manifest.messages_total, 3);
    assert_eq!(manifest.messages_below_frozen_floor, 2);
    assert_eq!(manifest.latest_user_message_index, Some(2));
}

#[test]
fn frozen_count_above_messages_clamps() {
    let b = body(json!({
        "messages": [{"role": "user", "content": "x"}]
    }));
    let out = compress_anthropic_live_zone(&b, 99, AuthMode::Payg, DEFAULT_MODEL).unwrap();
    let manifest = match &out {
        LiveZoneOutcome::NoChange { manifest } => manifest,
        _ => panic!("expected NoChange"),
    };
    assert_eq!(manifest.messages_below_frozen_floor, 1);
    assert_eq!(manifest.latest_user_message_index, None);
}

// ─── Manifest accessor helpers (consumed by PyO3 binding) ─────────

fn make_manifest(actions: Vec<BlockAction>) -> CompressionManifest {
    CompressionManifest {
        messages_total: actions.len(),
        messages_below_frozen_floor: 0,
        latest_user_message_index: None,
        block_outcomes: actions
            .into_iter()
            .enumerate()
            .map(|(i, a)| BlockOutcome {
                message_index: i,
                block_index: None,
                block_type: "test".to_string(),
                action: a,
            })
            .collect(),
    }
}

#[test]
fn tokens_saved_zero_for_empty_manifest() {
    let m = CompressionManifest::empty();
    assert_eq!(m.tokens_saved(), 0);
    assert!(m.transforms_applied().is_empty());
}

#[test]
fn tokens_saved_sums_compressed_outcomes_only() {
    let m = make_manifest(vec![
        BlockAction::Compressed {
            strategy: "smart_crusher",
            original_bytes: 0,
            compressed_bytes: 0,
            original_tokens: 100,
            compressed_tokens: 30,
        },
        BlockAction::NoCompressionApplied {
            content_type: "image".to_string(),
            declined_by: None,
        },
        BlockAction::Compressed {
            strategy: "log_compressor",
            original_bytes: 0,
            compressed_bytes: 0,
            original_tokens: 200,
            compressed_tokens: 50,
        },
        BlockAction::RejectedNotSmaller {
            strategy: "smart_crusher",
            original_bytes: 0,
            compressed_bytes: 0,
            original_tokens: 80,
            compressed_tokens: 90,
        },
    ]);
    // 70 + 150 = 220; rejected variant must not contribute.
    assert_eq!(m.tokens_saved(), 220);
}

#[test]
fn transforms_applied_dedup_first_seen_order() {
    let m = make_manifest(vec![
        BlockAction::Compressed {
            strategy: "log_compressor",
            original_bytes: 0,
            compressed_bytes: 0,
            original_tokens: 50,
            compressed_tokens: 10,
        },
        BlockAction::Compressed {
            strategy: "smart_crusher",
            original_bytes: 0,
            compressed_bytes: 0,
            original_tokens: 50,
            compressed_tokens: 10,
        },
        BlockAction::Compressed {
            strategy: "log_compressor",
            original_bytes: 0,
            compressed_bytes: 0,
            original_tokens: 50,
            compressed_tokens: 10,
        },
    ]);
    assert_eq!(
        m.transforms_applied(),
        vec!["log_compressor", "smart_crusher"]
    );
}

#[test]
fn tokens_saved_saturates_when_compressed_exceeds_original() {
    // Defensive — the dispatcher's RejectedNotSmaller gate should
    // make this unreachable, but the helper must not panic if a
    // future caller hand-constructs such a manifest.
    let m = make_manifest(vec![BlockAction::Compressed {
        strategy: "smart_crusher",
        original_bytes: 0,
        compressed_bytes: 0,
        original_tokens: 10,
        compressed_tokens: 50,
    }]);
    assert_eq!(m.tokens_saved(), 0);
}
