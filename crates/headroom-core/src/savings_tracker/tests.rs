use super::*;

fn tracker(path: &Path) -> SavingsTracker {
    SavingsTracker::new(Some(path.to_path_buf()), false)
}

/// The whole point of the durable metrics: an in-process watchdog resets
/// on restart, so cache behaviour has to survive a reload or there is no
/// baseline to compare against.
#[test]
fn cache_counters_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    {
        let t = tracker(&path);
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 1_000,
            tokens_saved: 400,
            attempted_input_tokens: 1_400,
            cache_read_tokens: 5_000,
            cache_write_tokens: 900,
            cache_write_1h_tokens: 900,
            cached: true,
            ..Default::default()
        });
        t.record_cache_miss(Some("anthropic"), Some("prefix_change"));
        t.record_cache_bust(2_500);
    }

    // Fresh tracker over the same file — this is the restart.
    let t = tracker(&path);
    let v = t.savings_verdict();
    assert_eq!(v["tokens_saved_by_compression"], 400);
    assert_eq!(v["tokens_lost_to_cache_busts"], 2_500);
    assert_eq!(v["bust_count"], 1);
    assert_eq!(v["prefix_change_misses"], 1);
    assert_eq!(v["cache_read_tokens"], 5_000);
    // Saved 400, made the provider rebuild 2,500. That is a loss, and the
    // verdict has to say so rather than reporting the 400 alone.
    assert_eq!(v["net_tokens_saved"], -2_100);
    assert_eq!(v["verdict"], "costing more than it saves");
}

/// Emitted output is priced into lifetime spend, checkpointed in history,
/// and recovered from it when the lifetime block lost it.
#[test]
fn output_spend_accumulates_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    let expected = estimate_output_savings_usd("claude-sonnet-4", 1_000);
    assert!(expected > 0.0);
    {
        let t = tracker(&path);
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 1_000,
            tokens_saved: 400,
            output_tokens: 1_000,
            ..Default::default()
        });
        let snap = t.snapshot();
        assert_eq!(
            snap["lifetime"]["total_output_cost_usd"],
            json!(round_n(expected, 6))
        );
    }

    // A lifetime block written before the field existed.
    let mut raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    raw["lifetime"]
        .as_object_mut()
        .unwrap()
        .remove("total_output_cost_usd");
    std::fs::write(&path, raw.to_string()).unwrap();

    let snap = tracker(&path).snapshot();
    assert_eq!(
        snap["lifetime"]["total_output_cost_usd"],
        json!(round_n(expected, 6))
    );
}

/// The cost of lossy compression that per-request token counts miss: the
/// model got a summary, could not work with it, and made the client resend
/// the full file. That shows up as extra turns, not as extra tokens on any
/// one turn, so `net_tokens_saved` alone would call it a win.
///
/// `parse_messages` detects it as `reread_compressed`, and the signal has
/// to survive all the way to the persisted blob to be worth anything.
#[test]
fn reread_of_compressed_content_is_counted_and_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    {
        let t = tracker(&path);
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 1_000,
            tokens_saved: 900,
            waste_signals: Some(vec![
                ("reread".to_string(), 5_000),
                ("reread_compressed".to_string(), 4_000),
            ]),
            ..Default::default()
        });
    }

    // Restart, then read it back off disk.
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        raw["lifetime_metrics"]["waste_signals"]["reread_compressed"],
        4_000
    );
    assert_eq!(raw["lifetime_metrics"]["waste_signals"]["reread"], 5_000);

    // And it is still there through a fresh tracker's load path.
    let t = tracker(&path);
    let snap = t.metrics_snapshot(&serde_json::json!({}));
    assert_eq!(snap["waste_signals"]["reread_compressed"], 4_000);
}

/// The proxy injects a retrieval tool, steering text and memory defs
/// *before* compression measures its baseline, so those bytes are invisible
/// to `tokens_saved`. This is the only place they are counted.
#[test]
fn proxy_overhead_is_counted_and_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    {
        let t = tracker(&path);
        t.record_proxy_overhead(1_000, 1_480); // injected 480 bytes
        t.record_proxy_overhead(1_480, 1_200); // compaction gave 280 back
    }
    let t = tracker(&path);
    let snap = t.proxy_overhead_report();
    assert_eq!(snap["added_bytes"], 480);
    assert_eq!(snap["removed_bytes"], 280);
    assert_eq!(snap["net_bytes"], 200);
    assert_eq!(snap["measured_requests"], 2);
    assert_eq!(snap["net_bytes_per_request"], 100);
}

/// The wire/usage pair is the only measurement that crosses the boundary
/// to the provider, so it has to survive a restart like the rest.
#[test]
fn wire_footprint_reconciles_bytes_against_provider_usage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    {
        let t = tracker(&path);
        t.record_wire_footprint(10_000, 6_000, 1_000, 8_000, 500);
        t.record_wire_footprint(10_000, 6_000, 1_000, 8_000, 500);
    }
    let t = tracker(&path);
    let snap = t.wire_verdict();
    assert_eq!(snap["bytes_in"], 20_000);
    assert_eq!(snap["bytes_out"], 12_000);
    assert_eq!(snap["bytes_saved"], 8_000);
    assert_eq!(snap["bytes_saved_percent"], 40.0);
    // Cache reads are free on a subscription, so "billed" is uncached
    // input plus creation only.
    assert_eq!(snap["provider_billed_tokens"], 3_000);
    assert_eq!(snap["bytes_per_billed_token"], 4.0);
    assert_eq!(snap["measured_requests"], 2);
}

/// The field exists to prove the state on disk is current. Left unstamped
/// it reported `null` forever while the file was being written every few
/// seconds, which reads as "persistence is broken" when persistence is
/// fine.
#[test]
fn a_successful_save_stamps_last_saved_at() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    let empty = serde_json::json!({});
    {
        let t = tracker(&path);
        assert!(
            t.metrics_snapshot(&empty)["persistence"]["last_saved_at"].is_null(),
            "nothing saved yet, so nothing to stamp"
        );
        t.record_proxy_overhead(5_000, 4_000);
        assert!(
            t.metrics_snapshot(&empty)["persistence"]["last_saved_at"].is_string(),
            "the write happened, so the stamp has to be there"
        );
    }
    // And it survives the round trip, so a restart can tell how stale the
    // state it just loaded is.
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(raw["lifetime_metrics"]["persistence"]["last_saved_at"].is_string());
}

/// `other` is the bucket the recorder falls back to for an unrecognised
/// waste label. It used to be missing from the loader's vocabulary, so
/// every restart folded the whole bucket into `unknown` — a bucket no
/// request is ever classified into. Left alone it grew without bound and
/// looked like a classifier failure.
#[test]
fn the_other_waste_bucket_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    {
        let t = tracker(&path);
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 1_000,
            waste_signals: Some(vec![("a_label_the_loader_never_heard_of".to_string(), 900)]),
            ..Default::default()
        });
    }
    // The recorder buckets it as `other` on the way out.
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["lifetime_metrics"]["waste_signals"]["other"], 900);

    // And the loader has to keep it there on the way back in.
    let t = tracker(&path);
    let waste = t.metrics_snapshot(&serde_json::json!({}))["waste_signals"].clone();
    assert_eq!(waste["other"], 900, "the fallback bucket has to round-trip");
    assert!(
        waste.get("unknown").is_none(),
        "nothing should land in `unknown`: {waste}"
    );
}

/// With no usage recorded the ratios must be absent, not zero: a zero here
/// reads as "we sent bytes and were billed nothing", which is a much
/// stronger claim than "we never measured".
#[test]
fn wire_verdict_reports_null_rather_than_zero_without_data() {
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("proxy_savings.json"));
    let snap = t.wire_verdict();
    assert!(snap["bytes_saved_percent"].is_null());
    assert!(snap["bytes_per_billed_token"].is_null());
    assert_eq!(snap["measured_requests"], 0);
}

/// A net shrink has to survive as a negative. Reporting it as zero would
/// hide compaction paying for the injections.
#[test]
fn a_net_shrink_reports_negative() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    {
        let t = tracker(&path);
        t.record_proxy_overhead(5_000, 4_000);
    }
    let t = tracker(&path);
    let snap = t.proxy_overhead_report();
    assert_eq!(snap["added_bytes"], 0);
    assert_eq!(snap["removed_bytes"], 1_000);
    assert_eq!(snap["net_bytes"], -1_000);
}

/// The measured case: most tools defined, few called. The report has to
/// name the expensive never-called ones and suggest whole MCP servers to
/// drop, because that is the decision an operator can actually make.
#[test]
fn never_called_tools_are_named_and_priced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    {
        let t = tracker(&path);
        let defs = vec![
            ("Read".to_string(), 500),
            ("Workflow".to_string(), 21_000),
            ("mcp__chrome__click".to_string(), 4_000),
            ("mcp__chrome__type".to_string(), 3_000),
            ("mcp__ctx__search".to_string(), 2_000),
        ];
        let calls = vec![
            ("Read".to_string(), 12),
            ("mcp__ctx__search".to_string(), 3),
        ];
        // Twice: definition sizes must not accumulate across turns.
        t.record_tools(&defs, &calls);
        t.record_tools(&defs, &calls);
    }

    let t = tracker(&path);
    let r = t.tool_inventory_report();
    assert_eq!(r["definition_bytes_total"], 30_500, "sizes must not double");
    assert_eq!(r["tools_defined"], 5);
    assert_eq!(r["tools_never_called"], 3);
    assert_eq!(r["never_called_bytes"], 28_000);
    assert_eq!(r["worst_offenders"][0]["name"], "Workflow");
    assert_eq!(r["worst_offenders"][0]["bytes"], 21_000);
    // chrome answered nothing; ctx did, so it must not be suggested.
    assert_eq!(
        r["drop_mcp_servers_suggestion"],
        serde_json::json!(["chrome"])
    );
}

/// Nothing to suggest must show as nothing, not as a reassuring empty
/// state that looks the same as "not measured".
#[test]
fn a_fully_used_install_suggests_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    let t = tracker(&path);
    t.record_tools(
        &[("mcp__ctx__search".to_string(), 2_000)],
        &[("mcp__ctx__search".to_string(), 3)],
    );
    let r = t.tool_inventory_report();
    assert_eq!(r["tools_never_called"], 0);
    assert_eq!(r["never_called_bytes"], 0);
    assert_eq!(r["drop_mcp_servers_suggestion"], serde_json::json!([]));
}

/// A tool absent from one request but sent by another client recently must
/// survive: subagents send narrower tool sets than the session that spawned
/// them.
#[test]
fn a_tool_missing_from_one_request_survives() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    let t = tracker(&path);
    t.record_tools(
        &[
            ("Bash".to_string(), 400),
            ("mcp__ctx__search".to_string(), 2_000),
        ],
        &[],
    );
    // A subagent turn carrying only Bash.
    t.record_tools(&[("Bash".to_string(), 400)], &[("Bash".to_string(), 1)]);

    let r = t.tool_inventory_report();
    assert_eq!(r["tools_defined"], 2);
    assert_eq!(r["tools_never_called"], 1);
}

/// A savings file written before the metrics existed must load, not throw
/// the user's history away.
#[test]
fn a_file_without_metrics_upgrades_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    std::fs::write(
        &path,
        r#"{"schema_version":3,"lifetime":{"requests":7,"tokens_saved":123},
                "display_session":{},"history":[],"projects":{}}"#,
    )
    .unwrap();

    let t = tracker(&path);
    let v = t.savings_verdict();
    assert_eq!(v["verdict"], "no data yet");
    assert_eq!(v["net_tokens_saved"], 0);
    // The pre-existing lifetime block is untouched.
    assert_eq!(t.snapshot()["lifetime"]["requests"], 7);
}

/// Compression that never busts anything is the shape we want reported as
/// a win.
#[test]
fn clean_compression_reads_as_saving() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    let t = tracker(&path);
    t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 1_000,
        tokens_saved: 900,
        attempted_input_tokens: 1_900,
        cache_read_tokens: 50_000,
        cached: true,
        ..Default::default()
    });
    let v = t.savings_verdict();
    assert_eq!(v["net_tokens_saved"], 900);
    assert_eq!(v["verdict"], "saving");
    assert_eq!(v["tokens_lost_to_cache_busts"], 0);
}

#[test]
fn record_request_updates_lifetime_session_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    let t = tracker(&path);
    assert!(t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 500,
        tokens_saved: 1000,
        provider: Some("anthropic"),
        project: Some("proj-a"),
        uncached_input_tokens: 500,
        ..Default::default()
    }));
    let snap = t.snapshot();
    assert_eq!(snap["lifetime"]["requests"], json!(1));
    assert_eq!(snap["lifetime"]["tokens_saved"], json!(1000));
    assert_eq!(snap["display_session"]["requests"], json!(1));
    assert_eq!(snap["history"].as_array().unwrap().len(), 1);
    assert_eq!(snap["projects"]["proj-a"]["tokens_saved"], json!(1000));
    assert!(path.exists());
}

#[test]
fn cache_only_turn_appends_history_point() {
    // Cache mode compresses nothing by design (`tokens_saved` 0), so the
    // old tokens-only gate dropped every history point on exactly the
    // turns cache mode exists for (Python `d1258055`).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    let t = tracker(&path);
    assert!(t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 50_000,
        tokens_saved: 0,
        cache_read_tokens: 50_000,
        cached: true,
        ..Default::default()
    }));
    let snap = t.snapshot();
    let history = snap["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["cache_read_tokens"], json!(50_000));
    assert_eq!(snap["lifetime"]["cache_read_tokens"], json!(50_000));

    // A turn with no savings from any mechanism still appends nothing.
    assert!(t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 100,
        ..Default::default()
    }));
    assert_eq!(t.snapshot()["history"].as_array().unwrap().len(), 1);
}

#[test]
fn request_scoped_savings_price_overrides_fresh_input_estimate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    let t = tracker(&path);
    t.record_request(&RequestRecord {
        model: "claude-opus-5",
        input_tokens: 1_000,
        tokens_saved: 1_000,
        // Cache-read pricing for 1k Opus tokens. The legacy fresh-input
        // estimate would be $0.015, ten times larger.
        compression_savings_cost_usd: Some(0.0015),
        ..Default::default()
    });

    assert_eq!(
        t.snapshot()["lifetime"]["compression_savings_usd"],
        json!(0.0015)
    );
}

#[test]
fn claude_cache_economics_include_read_discount_and_write_premium() {
    // Sonnet: fresh=$3/M, read=$0.30/M, 5m write=$3.75/M.
    // Read discount: $0.027; write premium: $0.0015; net: $0.0255.
    // No TTL split reported, so every write falls back to the 5m rate.
    let savings = estimate_cache_savings_usd("claude-sonnet-4", 10_000, 2_000, 0, 0, 3_000);
    assert!((savings - 0.0255).abs() < 1e-12, "got {savings}");
}

#[test]
fn cache_write_premium_can_make_net_savings_negative() {
    // Four thousand fresh Sonnet tokens would cost $0.012; creating the
    // cache entry costs $0.015, so this turn is a $0.003 cache loss.
    let savings = estimate_cache_savings_usd("claude-sonnet-4", 0, 4_000, 0, 0, 0);
    assert!((savings + 0.003).abs() < 1e-12, "got {savings}");
}

#[test]
fn unknown_model_does_not_invent_cache_savings() {
    assert_eq!(
        estimate_cache_savings_usd("unpriced-provider/model", 1_000_000, 0, 0, 0, 0),
        0.0
    );
}

#[test]
fn a_one_hour_write_costs_more_than_the_same_tokens_at_five_minutes() {
    // The regression that made `--force-1h-cache-ttl` free on paper: the
    // same 2k writes billed at 2.0x instead of 1.25x. Sonnet fresh=$3/M, so
    // all-fresh is $0.006 against $0.0075 (5m) and $0.012 (1h).
    let five_m = estimate_cache_savings_usd("claude-sonnet-4", 0, 2_000, 2_000, 0, 0);
    let one_h = estimate_cache_savings_usd("claude-sonnet-4", 0, 2_000, 0, 2_000, 0);
    assert!((five_m + 0.0015).abs() < 1e-12, "5m: got {five_m}");
    assert!((one_h + 0.006).abs() < 1e-12, "1h: got {one_h}");
    assert!(one_h < five_m, "a 1h write must book the larger premium");
}

#[test]
fn a_mixed_ttl_turn_prices_each_half_at_its_own_rate() {
    // 1k at $3.75/M + 3k at $6/M = $0.02175 against $0.012 all-fresh.
    let savings = estimate_cache_savings_usd("claude-sonnet-4", 0, 4_000, 1_000, 3_000, 0);
    assert!((savings + 0.00975).abs() < 1e-12, "got {savings}");
}

#[test]
fn writes_the_split_does_not_cover_fall_back_to_the_five_minute_rate() {
    // A route that reports a total but no split must not invent a premium:
    // the 2k residual joins the 1k of reported 5m at $3.75/M, and only the
    // measured 1k of 1h pays $6/M.
    let savings = estimate_cache_savings_usd("claude-sonnet-4", 0, 4_000, 1_000, 1_000, 0);
    assert!((savings + 0.00525).abs() < 1e-12, "got {savings}");
}

#[test]
fn input_cost_charges_one_hour_writes_at_the_one_hour_rate() {
    // 2k 1h writes = $0.012; the single-rate table would have said $0.0075.
    let cost = estimate_input_cost_usd("claude-sonnet-4", 0, 0, 2_000, 0, 2_000, 0);
    assert!((cost - 0.012).abs() < 1e-12, "got {cost}");
}

#[test]
fn cache_savings_survive_a_restart_in_lifetime_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    {
        let t = tracker(&path);
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 4_000,
            cache_write_tokens: 4_000,
            ..Default::default()
        });
        assert_eq!(
            t.metrics_snapshot(&json!({}))["cost"]["cache_savings_usd"],
            json!(-0.003)
        );
    }

    let restarted = tracker(&path);
    assert_eq!(
        restarted.metrics_snapshot(&json!({}))["cost"]["cache_savings_usd"],
        json!(-0.003)
    );
    assert_eq!(
        restarted.snapshot()["display_session"]["cache_savings_usd"],
        json!(-0.003)
    );
}

#[test]
fn display_session_without_cache_savings_migrates_to_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    let now = to_utc_iso(utc_now());
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "schema_version": SCHEMA_VERSION,
            "lifetime": {},
            "display_session": {
                "requests": 1,
                "tokens_saved": 1_000,
                "compression_savings_usd": 0.003,
                "total_input_tokens": 1_000,
                "total_input_cost_usd": 0.003,
                "savings_percent": 50.0,
                "started_at": now,
                "last_activity_at": now,
            },
            "history": [],
            "projects": {},
        }))
        .unwrap(),
    )
    .unwrap();

    let display = &tracker(&path).snapshot()["display_session"];
    assert_eq!(display["cache_savings_usd"], json!(0.0));
    assert_eq!(display["savings_percent"], json!(50.0));
}

/// The cache discount is reported and not claimed.
///
/// This test asserted the opposite until 2026-09-08, and the policy it
/// pinned made the headline figure meaningless: Claude Code sends its own
/// cache_control markers and gets the same 0.1x reads with no proxy in the
/// path, so counting the whole discount as a saving credits us with
/// Anthropic's cache. On one real session it read 87% where the measured
/// saving was under 1%.
#[test]
fn the_cache_discount_is_reported_but_not_counted_as_our_saving() {
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("s.json"));
    t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        // Actual: 1k read ($0.0003) + 1k uncached ($0.0030).
        input_tokens: 2_000,
        cache_read_tokens: 1_000,
        uncached_input_tokens: 1_000,
        // Compression avoided another 1k fresh tokens ($0.0030).
        tokens_saved: 1_000,
        ..Default::default()
    });

    let display = &t.snapshot()["display_session"];
    assert_eq!(display["compression_savings_usd"], json!(0.003));
    // Still reported: it is the right denominator for asking whether the
    // cache is working at all.
    assert_eq!(display["cache_savings_usd"], json!(0.0027));
    // But out of the percentage. $0.003 compression over $0.0033 actual
    // plus that same $0.003 -- what this request cost against what the
    // client would have paid sending the tokens we removed.
    assert_eq!(display["savings_percent"], json!(47.62));
}

#[test]
fn record_compression_savings_rejects_nonpositive() {
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("s.json"));
    assert!(!t.record_compression_savings("m", 0, None, None, None, None));
    assert!(!t.record_compression_savings("m", -5, None, None, None, None));
    assert!(t.record_compression_savings(
        "claude-sonnet-4",
        100,
        Some("anthropic"),
        None,
        None,
        None
    ));
}

#[test]
fn display_session_rolls_over_after_inactivity() {
    let dir = tempfile::tempdir().unwrap();
    let t = SavingsTracker::with_options(
        Some(dir.path().join("s.json")),
        DEFAULT_MAX_HISTORY_POINTS,
        DEFAULT_MAX_HISTORY_AGE_DAYS,
        DEFAULT_MAX_RESPONSE_HISTORY_POINTS,
        60,
        false,
    );
    // Anchor near real "now" so the snapshot's live-expiry check keeps the
    // second (current) session visible.
    let t1 = utc_now();
    let t0 = t1 - Duration::minutes(90);
    t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 100,
        tokens_saved: 100,
        timestamp: Some(t0),
        ..Default::default()
    });
    // 90 min later → new session (requests resets to 1).
    t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 100,
        tokens_saved: 100,
        timestamp: Some(t1),
        ..Default::default()
    });
    let snap = t.snapshot();
    assert_eq!(snap["display_session"]["requests"], json!(1));
    // Lifetime still accumulates across sessions.
    assert_eq!(snap["lifetime"]["requests"], json!(2));
}

#[test]
fn project_eviction_at_cap() {
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("s.json"));
    // 51 distinct projects; smallest gets evicted.
    for i in 0..=DEFAULT_MAX_PROJECTS {
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 10,
            tokens_saved: (i as i64) + 1, // strictly increasing saved
            project: Some(&format!("p{i:03}")),
            ..Default::default()
        });
    }
    let snap = t.snapshot();
    let projects = snap["projects"].as_object().unwrap();
    assert_eq!(projects.len(), DEFAULT_MAX_PROJECTS);
    // p000 had the smallest tokens_saved → evicted.
    assert!(!projects.contains_key("p000"));
}

#[test]
fn sanitize_project_name_cases() {
    assert_eq!(
        sanitize_project_name(Some("  my-proj  ")).as_deref(),
        Some("my-proj")
    );
    assert_eq!(
        sanitize_project_name(Some("caf%C3%A9")).as_deref(),
        Some("café")
    );
    assert!(sanitize_project_name(Some("   ")).is_none());
    assert!(sanitize_project_name(None).is_none());
    let long = "x".repeat(200);
    assert_eq!(
        sanitize_project_name(Some(&long)).unwrap().len(),
        PROJECT_NAME_MAX_LENGTH
    );
}

#[test]
fn save_load_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    {
        let t = tracker(&path);
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 500,
            tokens_saved: 1000,
            provider: Some("anthropic"),
            project: Some("proj-a"),
            ..Default::default()
        });
    }
    // Reload from disk.
    let t2 = tracker(&path);
    let snap = t2.snapshot();
    assert_eq!(snap["schema_version"], json!(4));
    assert_eq!(snap["lifetime"]["tokens_saved"], json!(1000));
    assert_eq!(snap["history"].as_array().unwrap().len(), 1);
    assert_eq!(snap["projects"]["proj-a"]["tokens_saved"], json!(1000));
}

#[test]
fn stateless_never_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.json");
    let t = SavingsTracker::new(Some(path.clone()), true);
    t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 100,
        tokens_saved: 100,
        ..Default::default()
    });
    // Live counters update in memory, but no file is written.
    assert_eq!(t.snapshot()["lifetime"]["tokens_saved"], json!(100));
    assert!(!path.exists());
}

/// A restart runs two proxies on one file while the old one drains. Only
/// the new one may write it, and what the old one records after the signal
/// must still reach the totals: once, at the time it happened.
#[test]
fn a_draining_proxy_hands_its_records_to_the_next_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy_savings.json");
    let turn = |saved| RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 100,
        tokens_saved: saved,
        ..Default::default()
    };

    let old = tracker(&path);
    old.record_request(&turn(1));
    old.record_request(&turn(2)); // inside the save interval: memory only
    old.begin_handoff();

    // The restart script starts the next proxy once the port is free.
    let new = tracker(&path);
    assert_eq!(new.snapshot()["lifetime"]["tokens_saved"], json!(3));

    let finished_at = utc_now() - Duration::minutes(3);
    old.record_request(&RequestRecord {
        timestamp: Some(finished_at),
        ..turn(10)
    });
    old.record_failed_work(&FailedWorkRecord {
        status_code: 529,
        ..Default::default()
    });
    assert_eq!(old.snapshot()["lifetime"]["tokens_saved"], json!(3));
    new.record_request(&turn(100));

    // A line torn by a crash mid-append must not cost the lines after it.
    OpenOptions::new()
        .append(true)
        .open(path.with_extension("handoff.jsonl"))
        .unwrap()
        .write_all(b"{\"request\":{\"mod\n")
        .unwrap();
    old.record_request(&turn(1000));

    assert_eq!(
        old.absorb_handoff(),
        0,
        "a draining proxy leaves the journal"
    );
    assert_eq!(new.absorb_handoff(), 3);
    assert_eq!(new.absorb_handoff(), 0, "an absorbed line is gone");

    let snap = new.snapshot();
    assert_eq!(snap["lifetime"]["tokens_saved"], json!(1113));
    assert_eq!(snap["lifetime"]["requests"], json!(5));
    assert_eq!(snap["failed_work"]["requests"], json!(1));
    let stamps: Vec<&str> = snap["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["timestamp"].as_str())
        .collect();
    assert!(stamps.contains(&to_utc_iso(finished_at).as_str()));

    // The old process exits last; its final flush must not win.
    new.flush();
    drop(old);
    assert_eq!(
        tracker(&path).snapshot()["lifetime"]["tokens_saved"],
        json!(1113)
    );
}

#[test]
fn history_response_rollups_and_summary() {
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("s.json"));
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 10, 30, 0).unwrap();
    for i in 0..3 {
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 100,
            tokens_saved: 100,
            provider: Some("anthropic"),
            timestamp: Some(t0 + Duration::minutes(i * 5)),
            ..Default::default()
        });
    }
    let resp = t.history_response("compact");
    // All three land in the same hourly bucket.
    let hourly = resp["series"]["hourly"].as_array().unwrap();
    assert_eq!(hourly.len(), 1);
    // Bucket delta = cumulative growth across the three checkpoints.
    assert_eq!(hourly[0]["tokens_saved"], json!(300));
    assert_eq!(resp["history_summary"]["stored_points"], json!(3));
    assert_eq!(resp["history_summary"]["compacted"], json!(false));
}

#[test]
fn export_csv_history_header() {
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("s.json"));
    t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 100,
        tokens_saved: 100,
        ..Default::default()
    });
    let csv = t.export_csv("history");
    let first_line = csv.lines().next().unwrap();
    assert_eq!(
        first_line,
        "timestamp,total_tokens_saved,compression_savings_usd,total_input_tokens,total_input_cost_usd"
    );
    // One data row.
    assert_eq!(csv.lines().count(), 2);
}

#[test]
fn stats_preview_shape() {
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("s.json"));
    let preview = t.stats_preview(20);
    assert_eq!(preview["schema_version"], json!(SCHEMA_VERSION));
    assert_eq!(preview["projects_limit"], json!(50));
    assert_eq!(preview["history_points"], json!(0));
}

#[test]
fn failed_work_is_durable_and_excluded_from_success_totals() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.json");
    {
        let t = tracker(&path);
        t.record_failed_work(&FailedWorkRecord {
            status_code: 529,
            upstream_attempts: 3,
            forwarded_tokens: 41_000,
            // Actual provider usage is intentionally a different number
            // from the request-side exposure estimate.
            provider_input_tokens: Some(38_500),
            provider_output_tokens: Some(17),
            ..Default::default()
        });
        t.record_failed_work(&FailedWorkRecord {
            status_code: 503,
            upstream_attempts: 2,
            forwarded_tokens: 10_000,
            // No usage block: never substitute forwarded tokens here.
            ..Default::default()
        });
    }

    let restarted = tracker(&path);
    let snap = restarted.snapshot();
    assert_eq!(snap["lifetime"]["requests"], 0);
    assert_eq!(snap["lifetime"]["tokens_saved"], 0);
    assert_eq!(snap["failed_work"]["requests"], 2);
    assert_eq!(snap["failed_work"]["upstream_attempts"], 5);
    assert_eq!(snap["failed_work"]["forwarded_tokens"], 51_000);
    assert_eq!(snap["failed_work"]["forwarded_tokens_at_risk"], 143_000);
    assert_eq!(
        snap["failed_work"]["provider_reported_input_tokens"],
        38_500
    );
    assert_eq!(snap["failed_work"]["provider_usage_observed_requests"], 1);
    assert_eq!(snap["failed_work"]["by_status"]["529"], 1);
    assert_eq!(snap["failed_work"]["by_status"]["503"], 1);
    let metrics = restarted.metrics_snapshot(&json!({}));
    assert_eq!(metrics["requests"]["total"], 0);
    assert_eq!(metrics["requests"]["failed"], 2);
}

#[test]
fn snapshot_schema_shape() {
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("s.json"));
    t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 1000,
        tokens_saved: 300,
        provider: Some("anthropic"),
        project: Some("test-proj"),
        cache_read_tokens: 200,
        uncached_input_tokens: 800,
        ..Default::default()
    });
    let snap = t.snapshot();

    // schema_version
    assert_eq!(snap["schema_version"], json!(SCHEMA_VERSION));

    // lifetime shape
    let lt = &snap["lifetime"];
    assert!(lt.is_object());
    for key in &[
        "requests",
        "tokens_saved",
        "compression_savings_usd",
        "total_input_tokens",
        "total_input_cost_usd",
        "output_tokens_saved",
        "output_savings_usd",
        "offload_savings_usd",
    ] {
        assert!(lt.get(*key).is_some(), "lifetime missing key: {key}");
    }

    // display_session shape
    let ds = &snap["display_session"];
    assert!(ds.is_object());
    for key in &[
        "requests",
        "tokens_saved",
        "compression_savings_usd",
        "cache_savings_usd",
        "offload_savings_usd",
        "total_input_tokens",
        "total_input_cost_usd",
        "savings_percent",
        "started_at",
        "last_activity_at",
    ] {
        assert!(ds.get(*key).is_some(), "display_session missing key: {key}");
    }

    // display_session_policy shape
    let dsp = &snap["display_session_policy"];
    assert!(dsp.is_object());
    assert!(dsp.get("rollover_inactivity_minutes").is_some());

    // history array of objects
    let hist = snap["history"].as_array().expect("history should be array");
    assert_eq!(hist.len(), 1);
    let entry = &hist[0];
    for key in &[
        "timestamp",
        "provider",
        "model",
        "total_tokens_saved",
        "compression_savings_usd",
        "total_input_tokens",
        "total_input_cost_usd",
    ] {
        assert!(
            entry.get(*key).is_some(),
            "history entry missing key: {key}"
        );
    }

    // retention shape
    let ret = &snap["retention"];
    assert!(ret.is_object());
    for key in &[
        "max_history_points",
        "max_history_age_days",
        "max_response_history_points",
    ] {
        assert!(ret.get(*key).is_some(), "retention missing key: {key}");
    }

    // projects is an object (keyed by project name)
    assert!(snap["projects"].is_object());

    // Cumulative values after one request.
    assert_eq!(snap["lifetime"]["requests"], json!(1));
    assert_eq!(snap["lifetime"]["tokens_saved"], json!(300));
    assert_eq!(snap["display_session"]["requests"], json!(1));
    // Compression only; the cache discount is reported beside it, not
    // folded in. See the_cache_discount_is_reported_but_not_counted.
    assert_eq!(snap["display_session"]["savings_percent"], json!(26.79));
}

#[test]
fn history_response_schema_shape() {
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("s.json"));
    let t0 = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
    for i in 0..2 {
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 200,
            tokens_saved: 50,
            provider: Some("anthropic"),
            timestamp: Some(t0 + Duration::minutes(i * 5)),
            ..Default::default()
        });
    }

    let resp = t.history_response("compact");

    // Top-level keys
    for key in &[
        "schema_version",
        "generated_at",
        "storage_path",
        "lifetime",
        "display_session",
        "display_session_policy",
        "history",
        "series",
        "exports",
        "retention",
        "projects",
        "history_summary",
    ] {
        assert!(resp.get(*key).is_some(), "response missing key: {key}");
    }

    // series sub-keys
    for bucket in &["hourly", "daily", "weekly", "monthly"] {
        assert!(
            resp["series"].get(*bucket).is_some(),
            "series missing bucket: {bucket}"
        );
        let arr = resp["series"][bucket]
            .as_array()
            .expect("series bucket should be array");
        if !arr.is_empty() {
            // Each rollup entry has the standard shape.
            let entry = &arr[0];
            for rk in &[
                "timestamp",
                "tokens_saved",
                "compression_savings_usd_delta",
                "total_tokens_saved",
                "compression_savings_usd",
                "total_input_tokens_delta",
                "total_input_tokens",
                "total_input_cost_usd_delta",
                "total_input_cost_usd",
                "output_tokens_saved_delta",
                "output_savings_usd_delta",
                "total_output_cost_usd_delta",
                "total_output_cost_usd",
                "by_provider",
                "by_model",
            ] {
                assert!(entry.get(*rk).is_some(), "rollup entry missing key: {rk}");
            }
        }
    }

    // exports shape
    assert!(resp["exports"]["available_formats"].is_array());
    assert!(resp["exports"]["available_series"].is_array());

    // history_summary shape
    for key in &["mode", "stored_points", "returned_points", "compacted"] {
        assert!(
            resp["history_summary"].get(*key).is_some(),
            "history_summary missing key: {key}"
        );
    }
}

// ─── Output-shaping savings (upstream addition) ──────────────────────

#[test]
fn output_savings_accumulate_separately_from_input_savings() {
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("s.json"));
    assert!(t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 500,
        tokens_saved: 1000,
        output_tokens_saved: 200,
        ..Default::default()
    }));
    let snap = t.snapshot();
    // Input-side and output-side savings must never be conflated: they are
    // different token streams priced at different rates.
    assert_eq!(snap["lifetime"]["tokens_saved"], json!(1000));
    assert_eq!(snap["lifetime"]["output_tokens_saved"], json!(200));
    // 200 output tokens at claude-sonnet-4's $15/1M output rate.
    let usd = snap["lifetime"]["output_savings_usd"].as_f64().unwrap();
    assert!((usd - 0.003).abs() < 1e-9, "got {usd}");
    // Input savings still priced at the $3/1M input rate.
    let in_usd = snap["lifetime"]["compression_savings_usd"]
        .as_f64()
        .unwrap();
    assert!((in_usd - 0.003).abs() < 1e-9, "got {in_usd}");
}

#[test]
fn output_savings_are_clamped_and_default_to_zero() {
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("s.json"));
    // A negative estimate must never subtract from the rollup.
    assert!(t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 10,
        tokens_saved: 10,
        output_tokens_saved: -50,
        ..Default::default()
    }));
    // And a request with no shaping simply contributes nothing.
    assert!(t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 10,
        tokens_saved: 10,
        ..Default::default()
    }));
    let snap = t.snapshot();
    assert_eq!(snap["lifetime"]["output_tokens_saved"], json!(0));
    assert_eq!(snap["lifetime"]["output_savings_usd"], json!(0.0));
}

#[test]
fn rollup_buckets_carry_output_deltas_like_python() {
    // Slice #2: per-bucket output rollup mirrors Python `_build_rollup`.
    // Two shaped requests in one hour bucket accumulate; the clamp
    // (`max(total - prev, 0)`) means a later shrink never subtracts.
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("s.json"));
    assert!(t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 500,
        tokens_saved: 1000,
        output_tokens_saved: 200,
        ..Default::default()
    }));
    assert!(t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 500,
        tokens_saved: 1000,
        output_tokens_saved: 100,
        ..Default::default()
    }));
    let resp = t.history_response("compact");
    let hourly = resp["series"]["hourly"].as_array().unwrap();
    assert!(!hourly.is_empty());
    let bucket = &hourly[0];
    assert_eq!(bucket["output_tokens_saved_delta"], json!(300));
    // 300 output tokens at claude-sonnet-4's $15/1M output rate.
    let usd = bucket["output_savings_usd_delta"].as_f64().unwrap();
    assert!((usd - 0.0045).abs() < 1e-9, "got {usd}");
    // Matches the lifetime totals: the rollup must reconcile with them.
    let snap = t.snapshot();
    assert_eq!(
        snap["lifetime"]["output_tokens_saved"],
        bucket["output_tokens_saved_delta"]
    );
}

#[test]
fn output_savings_estimator_matches_python() {
    // Known model: $15/1M output. Unknown model: same blended fallback.
    assert!((estimate_output_savings_usd("claude-sonnet-4", 1000) - 0.015).abs() < 1e-9);
    assert!((estimate_output_savings_usd("totally-unknown-model", 1000) - 0.015).abs() < 1e-9);
    assert_eq!(estimate_output_savings_usd("claude-sonnet-4", 0), 0.0);
    assert_eq!(estimate_output_savings_usd("claude-sonnet-4", -5), 0.0);
}

#[test]
fn a_free_model_is_not_billed_the_fallback_rate() {
    // Regression for the phantom-savings bug: filtering the looked-up rate
    // on `> 0.0` treated a legitimately free model as "price unknown" and
    // charged the blended fallback, inventing savings for something that
    // costs nothing. A model IN the table is priced at its own rate,
    // whatever that rate is.
    let priced = crate::pricing::lookup("claude-sonnet-4").expect("table entry");
    assert!(priced.output_cost_per_token > 0.0);
    // The estimator must use the table rate, not the fallback, whenever the
    // lookup succeeds.
    let expected = 1000.0 * priced.output_cost_per_token;
    assert!((estimate_output_savings_usd("claude-sonnet-4", 1000) - expected).abs() < 1e-12);
    let in_priced = 1000.0 * priced.input_cost_per_token;
    assert!((estimate_compression_savings_usd("claude-sonnet-4", 1000) - in_priced).abs() < 1e-12);
}

#[test]
fn a_free_model_costs_zero_input_not_fallback() {
    // Companion to `a_free_model_is_not_billed_the_fallback_rate` for the
    // input-cost path: `estimate_input_cost_usd` filtered the lookup on
    // `> 0.0`, so Spark turns were billed at the $3/M fallback in
    // `proxy_savings.json` while the cost tracker correctly reported 0.
    assert_eq!(
        estimate_input_cost_usd("muse-spark-1.3-contributor-free", 1000, 0, 0, 0, 0, 0),
        0.0
    );
    assert_eq!(
        estimate_input_cost_usd("claude-muse-spark-1.3", 1000, 0, 0, 0, 0, 0),
        0.0
    );
    // Unknown models still fail open to the blended fallback.
    assert!(
        (estimate_input_cost_usd("totally-unknown-model", 1000, 0, 0, 0, 0, 0)
            - 1000.0 * DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN)
            .abs()
            < 1e-12
    );
}

#[test]
fn router_offload_counts_as_saving() {
    // Serving a turn on a cheaper model than requested is a saving vs
    // potential consumption: it lands in lifetime/session totals and in
    // `savings_percent`, kept separate from compression savings.
    let dir = tempfile::tempdir().unwrap();
    let t = tracker(&dir.path().join("s.json"));
    t.record_request(&RequestRecord {
        model: "muse-spark-1.3-contributor-free",
        input_tokens: 1000,
        offload_savings_usd: 2.5,
        ..Default::default()
    });
    let snap = t.snapshot();
    assert_eq!(snap["lifetime"]["offload_savings_usd"], json!(2.5));
    assert_eq!(snap["display_session"]["offload_savings_usd"], json!(2.5));
    // Billed cost is 0 (free serving model, fixed above), compression 0:
    // the avoided $2.50 is the whole counterfactual → 100%.
    assert_eq!(snap["lifetime"]["total_input_cost_usd"], json!(0.0));
    assert_eq!(snap["display_session"]["savings_percent"], json!(100.0));
}

#[test]
fn lifetime_loads_from_a_state_file_written_before_output_shaping() {
    // Older state files have no output_* keys; they must load with zeros
    // rather than being rejected.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.json");
    let t = tracker(&path);
    t.record_request(&RequestRecord {
        model: "claude-sonnet-4",
        input_tokens: 100,
        tokens_saved: 50,
        output_tokens_saved: 25,
        ..Default::default()
    });
    let snap = t.snapshot();
    assert_eq!(snap["lifetime"]["output_tokens_saved"], json!(25));
}
