use super::*;
use serde_json::json;

// --- bash_program ---

#[test]
fn bash_program_simple() {
    let (prog, args) = bash_program("grep foo bar.txt");
    assert_eq!(prog, "grep");
    assert_eq!(args, vec!["foo", "bar.txt"]);
}

#[test]
fn bash_program_with_wrapper() {
    let (prog, args) = bash_program("rtk grep pattern");
    assert_eq!(prog, "grep");
    assert_eq!(args, vec!["pattern"]);
}

#[test]
fn bash_program_with_env() {
    let (prog, args) = bash_program("FOO=1 BAR=2 grep pattern");
    assert_eq!(prog, "grep");
    assert_eq!(args, vec!["pattern"]);
}

#[test]
fn bash_program_timeout_with_args() {
    let (prog, args) = bash_program("timeout 30 rg pattern");
    assert_eq!(prog, "rg");
    assert_eq!(args, vec!["pattern"]);
}

#[test]
fn bash_program_full_path() {
    let (prog, _args) = bash_program("/usr/bin/grep pattern");
    assert_eq!(prog, "grep");
}

// --- bash_command_is_search ---

#[test]
fn bash_command_is_search_true() {
    let cmds: HashSet<&str> = ["grep", "rg", "ripgrep", "ag", "ack"]
        .iter()
        .copied()
        .collect();
    assert!(bash_command_is_search("grep pattern", &cmds));
    assert!(bash_command_is_search("rg --json pattern", &cmds));
    assert!(bash_command_is_search("git grep pattern", &cmds));
}

#[test]
fn bash_command_is_search_false() {
    let cmds: HashSet<&str> = ["grep", "rg"].iter().copied().collect();
    assert!(!bash_command_is_search("cat file.txt", &cmds));
    assert!(!bash_command_is_search("ls -la", &cmds));
}

#[test]
fn bash_command_is_search_bash_c() {
    let cmds: HashSet<&str> = ["grep", "rg"].iter().copied().collect();
    assert!(bash_command_is_search("bash -c 'grep pattern file'", &cmds));
}

// --- is_mixed_content ---

#[test]
fn is_mixed_content_true() {
    let content = "# Title\n\n```python\ndef foo(): pass\n```\n\nSome prose here about something.\nMore prose.\nAnd more.\nAnd more.\nAnd more.\nAnd more.\nsrc/file.py:42: code";
    assert!(is_mixed_content(content));
}

#[test]
fn is_mixed_content_false_pure_code() {
    let content = "def foo():\n    pass\n\ndef bar():\n    pass";
    assert!(!is_mixed_content(content));
}

#[test]
fn is_mixed_content_false_pure_json() {
    let content = r#"[{"key": "value"}]"#;
    assert!(!is_mixed_content(content));
}

// --- json_shape ---

#[test]
fn json_shape_object() {
    let shape = json_shape(r#"{"a": 1, "b": 2}"#);
    assert_eq!(shape["is_json"], true);
    assert_eq!(shape["kind"], "object");
    assert_eq!(shape["length"], 2);
}

#[test]
fn json_shape_array() {
    let shape = json_shape("[1, 2, 3]");
    assert_eq!(shape["is_json"], true);
    assert_eq!(shape["kind"], "array");
    assert_eq!(shape["length"], 3);
}

#[test]
fn json_shape_invalid() {
    let shape = json_shape("not json");
    assert_eq!(shape["is_json"], false);
}

// --- gain_bucket ---

#[test]
fn gain_bucket_zero() {
    assert_eq!(gain_bucket(0.0), "0");
}

#[test]
fn gain_bucket_small_positive() {
    assert_eq!(gain_bucket(50.0), "lt100");
}

#[test]
fn gain_bucket_small_negative() {
    assert_eq!(gain_bucket(-50.0), "neg_lt100");
}

#[test]
fn gain_bucket_medium() {
    assert_eq!(gain_bucket(500.0), "lt1k");
}

#[test]
fn gain_bucket_large() {
    assert_eq!(gain_bucket(5000.0), "lt10k");
}

#[test]
fn gain_bucket_very_large() {
    assert_eq!(gain_bucket(50000.0), "gte10k");
}

#[test]
fn gain_bucket_nan() {
    assert_eq!(gain_bucket(f64::NAN), "nan");
}

// --- RoutingDecision ---

#[test]
fn routing_decision_ratio() {
    let d = RoutingDecision {
        content_type: ContentType::PlainText,
        strategy: CompressionStrategy::Kompress,
        original_tokens: 100,
        compressed_tokens: 50,
        confidence: 1.0,
        section_index: 0,
    };
    assert_eq!(d.compression_ratio(), 0.5);
}

#[test]
fn routing_decision_ratio_zero_original() {
    let d = RoutingDecision {
        content_type: ContentType::PlainText,
        strategy: CompressionStrategy::Kompress,
        original_tokens: 0,
        compressed_tokens: 0,
        confidence: 1.0,
        section_index: 0,
    };
    assert_eq!(d.compression_ratio(), 1.0);
}

// --- RouterCompressionResult ---

#[test]
fn router_result_totals() {
    let r = RouterCompressionResult {
        compressed: "a b".to_string(),
        original: "a b c d".to_string(),
        strategy_used: CompressionStrategy::Mixed,
        routing_log: vec![
            RoutingDecision {
                content_type: ContentType::PlainText,
                strategy: CompressionStrategy::Kompress,
                original_tokens: 100,
                compressed_tokens: 50,
                confidence: 1.0,
                section_index: 0,
            },
            RoutingDecision {
                content_type: ContentType::JsonArray,
                strategy: CompressionStrategy::SmartCrusher,
                original_tokens: 200,
                compressed_tokens: 80,
                confidence: 1.0,
                section_index: 1,
            },
        ],
        sections_processed: 2,
        strategy_chain: vec!["kompress".to_string(), "smart_crusher".to_string()],
        cache_hit: false,
    };
    assert_eq!(r.total_original_tokens(), 300);
    assert_eq!(r.total_compressed_tokens(), 130);
    assert!((r.compression_ratio() - 0.433).abs() < 0.01);
    assert_eq!(r.tokens_saved(), 170);
}

#[test]
fn router_result_summary_mixed() {
    let r = RouterCompressionResult {
        compressed: String::new(),
        original: String::new(),
        strategy_used: CompressionStrategy::Mixed,
        routing_log: vec![],
        sections_processed: 3,
        strategy_chain: vec![],
        cache_hit: false,
    };
    let s = r.summary();
    assert!(s.contains("Mixed content"));
    assert!(s.contains("3 sections"));
}

#[test]
fn router_result_summary_pure() {
    let r = RouterCompressionResult {
        compressed: String::new(),
        original: String::new(),
        strategy_used: CompressionStrategy::Search,
        routing_log: vec![RoutingDecision {
            content_type: ContentType::SearchResults,
            strategy: CompressionStrategy::Search,
            original_tokens: 200,
            compressed_tokens: 100,
            confidence: 1.0,
            section_index: 0,
        }],
        sections_processed: 1,
        strategy_chain: vec![],
        cache_hit: false,
    };
    let s = r.summary();
    assert!(s.contains("Pure search"));
}

// --- CompressionCache ---

#[test]
fn cache_put_and_get() {
    let cache = CompressionCache::new(1800);
    cache.put(42, "compressed text", 0.5, "kompress");

    match cache.get(42) {
        CacheLookup::Hit {
            compressed,
            ratio,
            strategy,
        } => {
            assert_eq!(compressed, "compressed text");
            assert_eq!(ratio, 0.5);
            assert_eq!(strategy, "kompress");
        }
        _ => panic!("expected cache hit"),
    }
}

#[test]
fn cache_miss() {
    let cache = CompressionCache::new(1800);
    assert!(matches!(cache.get(999), CacheLookup::Miss));
}

#[test]
fn cache_skip() {
    let cache = CompressionCache::new(1800);
    cache.mark_skip(42);
    assert!(cache.is_skipped(42));
    assert!(matches!(cache.get(42), CacheLookup::Skip));
}

#[test]
fn cache_move_to_skip() {
    let cache = CompressionCache::new(1800);
    cache.put(42, "compressed", 0.5, "kompress");
    cache.move_to_skip(42);
    assert!(!matches!(cache.get(42), CacheLookup::Hit { .. }));
    assert!(cache.is_skipped(42));
}

#[test]
fn cache_stats() {
    let cache = CompressionCache::new(1800);
    cache.put(1, "a", 0.5, "log");
    cache.put(2, "b", 0.6, "search");
    cache.mark_skip(3);

    let _ = cache.get(1); // hit
    let _ = cache.get(999); // miss
    let _ = cache.get(3); // skip hit

    let stats = cache.stats();
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.skip_hits, 1);
    assert_eq!(stats.size, 2);
    assert_eq!(stats.skip_size, 1);
}

#[test]
fn cache_clear() {
    let cache = CompressionCache::new(1800);
    cache.put(1, "a", 0.5, "log");
    cache.mark_skip(2);
    cache.clear();
    assert_eq!(cache.size(), 0);
    assert_eq!(cache.skip_size(), 0);
}

// --- tool_call_args_text ---

#[test]
fn tool_call_args_text_string() {
    let raw = json!("grep pattern file.txt");
    assert_eq!(tool_call_args_text(&raw), "grep pattern file.txt");
}

#[test]
fn tool_call_args_text_dict() {
    let raw = json!({"command": "grep pattern", "path": "/tmp"});
    let text = tool_call_args_text(&raw);
    assert!(text.contains("grep pattern"));
    assert!(text.contains("/tmp"));
}

#[test]
fn tool_call_args_text_capped() {
    let long = "word ".repeat(200);
    let raw = json!(long);
    let text = tool_call_args_text(&raw);
    assert!(text.len() <= 300);
}

#[test]
fn tool_call_args_text_non_scalar_values() {
    let raw = json!({"nested": {"key": "val"}, "simple": "ok"});
    let text = tool_call_args_text(&raw);
    assert!(text.contains("ok"));
    assert!(!text.contains("nested"));
}

// --- tool_call_command_text ---

#[test]
fn tool_call_command_text_dict() {
    let raw = json!({"command": "grep pattern"});
    assert_eq!(tool_call_command_text(&raw), "grep pattern");
}

#[test]
fn tool_call_command_text_json_string() {
    let raw = json!(r#"{"command": "ls -la"}"#);
    assert_eq!(tool_call_command_text(&raw), "ls -la");
}

#[test]
fn tool_call_command_text_array_command() {
    let raw = json!({"command": ["grep", "-r", "pattern"]});
    assert_eq!(tool_call_command_text(&raw), "grep -r pattern");
}

#[test]
fn tool_call_command_text_no_command() {
    let raw = json!({"name": "read_file"});
    assert_eq!(tool_call_command_text(&raw), "");
}

#[test]
fn tool_call_command_text_invalid_json_string() {
    let raw = json!("not json at all");
    assert_eq!(tool_call_command_text(&raw), "");
}

// --- strip_detection_envelope ---

#[test]
fn strip_detection_envelope_output() {
    let content = "<output>\nline1\nline2\n</output>";
    let stripped = strip_detection_envelope(content);
    assert_eq!(stripped, "line1\nline2");
}

#[test]
fn strip_detection_envelope_with_returncode() {
    let content = "<returncode>0</returncode>\n<output>result</output>";
    let stripped = strip_detection_envelope(content);
    assert_eq!(stripped, "result");
}

#[test]
fn strip_detection_envelope_no_tag() {
    let content = "just plain text";
    assert_eq!(strip_detection_envelope(content), content);
}

#[test]
fn strip_detection_envelope_partial_match() {
    let content = "before <output>middle</output> after";
    assert_eq!(strip_detection_envelope(content), content);
}

#[test]
fn strip_detection_envelope_empty_body() {
    let content = "<output>\n</output>";
    assert_eq!(strip_detection_envelope(content), content);
}

// --- extract_json_block ---

#[test]
fn extract_json_block_array() {
    let lines = vec!["[1,", "  2,", "  3]"];
    let (content, end) = extract_json_block(&lines, 0);
    assert!(content.is_some());
    assert_eq!(end, 2);
}

#[test]
fn extract_json_block_object() {
    let lines = vec![r#"{"key": "value","#, r#"  "num": 42}"#];
    let (content, end) = extract_json_block(&lines, 0);
    assert!(content.is_some());
    assert_eq!(end, 1);
}

#[test]
fn extract_json_block_nested() {
    let lines = vec![r#"{"a": [1, 2],"#, r#"  "b": {"c": 3}}"#];
    let (content, end) = extract_json_block(&lines, 0);
    assert!(content.is_some());
    assert_eq!(end, 1);
}

#[test]
fn extract_json_block_string_with_brackets() {
    let lines = vec![r#"{"path": "a]b"}"#];
    let (content, end) = extract_json_block(&lines, 0);
    assert!(content.is_some());
    assert_eq!(end, 0);
}

#[test]
fn extract_json_block_incomplete() {
    let lines = vec!["[1,", "  2,"];
    let (content, end) = extract_json_block(&lines, 0);
    assert!(content.is_none());
    assert_eq!(end, 0);
}

// --- split_into_sections ---

#[test]
fn split_into_sections_pure_code() {
    let content = "def foo():\n    pass\n\ndef bar():\n    pass";
    let sections = split_into_sections(content);
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].content_type, ContentType::PlainText);
}

#[test]
fn split_into_sections_code_fence() {
    let content = "# Title\n\n```python\ndef foo(): pass\n```\n\nMore text";
    let sections = split_into_sections(content);
    assert!(sections.len() >= 2);
    let code = sections
        .iter()
        .find(|s| s.content_type == ContentType::SourceCode);
    assert!(code.is_some());
    assert_eq!(code.unwrap().language.as_deref(), Some("python"));
}

#[test]
fn split_into_sections_search_results() {
    let content = "src/a.py:42: code\nsrc/b.py:10: other";
    let sections = split_into_sections(content);
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].content_type, ContentType::SearchResults);
}

#[test]
fn split_into_sections_carves_grep_context_lines() {
    // Upstream #3599: match lines and -A/-B/-C context lines carve as
    // search sections so code in them avoids the prose path.
    let content = "src/a.py:42:def f():\nsrc/a.py-43-    return 1\nsrc/b.py:10: other";
    let sections = split_into_sections(content);
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].content_type, ContentType::SearchResults);
}

#[test]
fn split_into_sections_carves_colon_dash_context_lines() {
    // The `path:NN-content` context shape the detector claims must carve
    // too, or it strands in prose/code sections (the #3599 misroute for
    // that shape).
    let content = "src/a.py:42:def f():\nsrc/a.py:43-    return 1\nsrc/b.py:10: other";
    let sections = split_into_sections(content);
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].content_type, ContentType::SearchResults);
}

#[test]
fn split_into_sections_mixed() {
    let content = "# Title\n\n```python\ndef foo(): pass\n```\n\nsrc/a.py:42: code";
    let sections = split_into_sections(content);
    assert!(sections.len() >= 2);
    let has_code = sections
        .iter()
        .any(|s| s.content_type == ContentType::SourceCode);
    let has_search = sections
        .iter()
        .any(|s| s.content_type == ContentType::SearchResults);
    assert!(has_code || has_search);
}

#[test]
fn split_into_sections_json() {
    let content = "text\n\n[1, 2, 3]\n\nmore text";
    let sections = split_into_sections(content);
    assert!(sections.len() >= 2);
    let json = sections
        .iter()
        .find(|s| s.content_type == ContentType::JsonArray);
    assert!(json.is_some());
}

// --- ContentRouterConfig ---

#[test]
// A flat table of asserts: each one scores as a branch, none nests.
#[allow(clippy::cognitive_complexity)]
fn config_default_values() {
    let config = ContentRouterConfig::default();
    assert!(!config.enable_code_aware);
    assert!(config.enable_kompress);
    assert!(config.enable_smart_crusher);
    assert!(config.enable_search_compressor);
    assert!(config.enable_log_compressor);
    assert!(config.enable_tabular_compressor);
    assert!(config.enable_html_extractor);
    assert!(config.enable_image_optimizer);
    assert!(config.prefer_code_aware_for_code);
    assert!(!config.force_kompress_all);
    assert!(!config.lossless);
    assert_eq!(config.min_section_tokens, 20);
    assert_eq!(config.fallback_strategy, CompressionStrategy::Kompress);
    assert!(config.skip_user_messages);
    assert_eq!(config.protect_recent_code, 4);
    assert!(config.protect_analysis_context);
    assert!(config.protect_error_outputs);
    assert_eq!(config.error_protection_max_chars, 8000);
    assert!(!config.compress_assistant_text_blocks);
    assert_eq!(config.min_chars_for_block_compression, 500);
    assert_eq!(config.protect_recent_reads_fraction, 0.0);
    assert_eq!(config.min_ratio_relaxed, 1.0);
    assert_eq!(config.min_ratio_aggressive, 1.0);
    assert!(config.ccr_enabled);
    assert!(config.ccr_inject_marker);
    assert!(config.smart_crusher_max_items_after_crush.is_none());
    assert!(config.smart_crusher_with_compaction);
    assert!(config.smart_crusher_lossless_only.is_none());
    assert!(config.relevance_split);
    assert_eq!(config.relevance_max_records, 0);
    assert!(config.relevance_adaptive_threshold);
    assert!(!config.compress_tagged_content);
    assert!(config.exclude_tools.is_none());
    assert!(config.bash_tool_names.contains("bash"));
    assert!(config.bash_search_commands.contains("grep"));
    assert!(!config.search_group_by_file);
}

// --- create_content_signature ---

#[test]
fn create_content_signature_deterministic() {
    let sig1 = create_content_signature("search", "file content", None);
    let sig2 = create_content_signature("search", "file content", None);
    assert_eq!(sig1, sig2);
    assert_eq!(sig1.len(), 24);
}

#[test]
fn create_content_signature_differs_by_type() {
    let sig1 = create_content_signature("search", "content", None);
    let sig2 = create_content_signature("log", "content", None);
    assert_ne!(sig1, sig2);
}

#[test]
fn create_content_signature_differs_by_language() {
    let sig1 = create_content_signature("code", "content", Some("python"));
    let sig2 = create_content_signature("code", "content", Some("rust"));
    assert_ne!(sig1, sig2);
}

#[test]
fn create_content_signature_empty_content() {
    let sig = create_content_signature("text", "", None);
    assert_eq!(sig.len(), 24);
}

// --- netcost_message_tokens ---

#[test]
fn netcost_message_tokens_string() {
    let content = json!("hello world foo bar");
    assert_eq!(netcost_message_tokens(&content), 4);
}

#[test]
fn netcost_message_tokens_text_block() {
    let content = json!([{"type": "text", "text": "hello world"}]);
    assert_eq!(netcost_message_tokens(&content), 2);
}

#[test]
fn netcost_message_tokens_tool_result_string() {
    let content = json!([{"type": "tool_result", "content": "hello world"}]);
    assert_eq!(netcost_message_tokens(&content), 2);
}

#[test]
fn netcost_message_tokens_tool_result_array() {
    let content = json!([
        {"type": "tool_result", "content": [
            {"type": "text", "text": "hello world"},
            {"type": "text", "text": "foo bar"}
        ]}
    ]);
    assert_eq!(netcost_message_tokens(&content), 4);
}

#[test]
fn netcost_message_tokens_mixed_blocks() {
    let content = json!([
        {"type": "text", "text": "hello"},
        {"type": "tool_result", "content": "world"}
    ]);
    assert_eq!(netcost_message_tokens(&content), 2);
}

#[test]
fn netcost_message_tokens_empty() {
    let content = json!("");
    assert_eq!(netcost_message_tokens(&content), 0);
}

#[test]
fn netcost_message_tokens_null() {
    let content = json!(null);
    assert_eq!(netcost_message_tokens(&content), 0);
}

// --- ToolSignature ---

#[test]
fn tool_signature_from_json_object() {
    let json = json!({"name": "Alice", "age": 30, "active": true});
    let sig = ToolSignature::from_items(std::slice::from_ref(&json));
    assert_eq!(sig.field_count, 3);
    assert!(!sig.has_nested_objects);
    assert!(!sig.has_arrays);
    assert_eq!(sig.max_depth, 1); // Flat object has depth 1
    assert_eq!(sig.structure_hash.len(), 24);
}

#[test]
fn tool_signature_from_json_nested() {
    let json = json!({"user": {"name": "Alice", "address": {"city": "NYC"}}});
    let sig = ToolSignature::from_items(std::slice::from_ref(&json));
    assert!(sig.has_nested_objects);
    assert!(sig.max_depth >= 2);
}

#[test]
fn tool_signature_from_json_array() {
    let json = json!({"items": [1, 2, 3]});
    let sig = ToolSignature::from_items(std::slice::from_ref(&json));
    assert!(sig.has_arrays);
}

#[test]
fn tool_signature_deterministic() {
    let json = json!({"key": "value"});
    let sig1 = ToolSignature::from_items(std::slice::from_ref(&json));
    let sig2 = ToolSignature::from_items(std::slice::from_ref(&json));
    assert_eq!(sig1.structure_hash, sig2.structure_hash);
}

#[test]
fn tool_signature_empty_is_deterministic() {
    // FINDING-013: empty input used to mint a wall-clock hash.
    let sig1 = ToolSignature::from_items(&[]);
    let sig2 = ToolSignature::from_items(&[]);
    assert_eq!(sig1.structure_hash, sig2.structure_hash);
}

#[test]
fn tool_signature_for_content_type() {
    let sig = ToolSignature::for_content_type("search", "content", None);
    assert_eq!(sig.field_count, 0);
    assert_eq!(sig.structure_hash.len(), 24);
}

// --- detect_content_native ---

#[test]
fn detect_content_native_json() {
    let content = r#"[{"id": 1}, {"id": 2}]"#;
    let ct = detect_content_native(content);
    assert_eq!(ct, ContentType::JsonArray);
}

#[test]
fn detect_content_native_code() {
    let content = "def foo():\n    pass\n\ndef bar():\n    pass\n\ndef baz():\n    pass\n\ndef qux():\n    return 42";
    let ct = detect_content_native(content);
    assert_eq!(ct, ContentType::SourceCode);
}

#[test]
fn detect_content_native_search() {
    let content = "src/main.py:42: def process():\nsrc/main.py:43:     return None";
    let ct = detect_content_native(content);
    assert_eq!(ct, ContentType::SearchResults);
}

/// Regression (2026-08-23): interactive wrap-copilot prompts were deleted.
/// Copilot CLI prepends `<current_datetime>…</current_datetime>` to every
/// interactive user turn; the ISO timestamp matched the grep `file:line:`
/// detector, the one-line prompt classified as search results, and the
/// SearchCompressor kept only the datetime line — the model received no
/// request and answered "How can I help you today?".
#[test]
fn detect_content_native_datetime_prefixed_prompt() {
    let content = "<current_datetime>2026-08-23T09:57:59.792+02:00</current_datetime>\n\n\
                       Please update the PR desc and check .overlay/ for hints.";
    let ct = detect_content_native(content);
    assert_ne!(ct, ContentType::SearchResults);
    assert_ne!(
        strategy_from_detection(ct, true),
        CompressionStrategy::Search
    );
}

#[test]
fn detect_content_native_diff() {
    let content = "diff --git a/f.py b/f.py\nindex 123..456 100644\n--- a/f.py\n+++ b/f.py\n@@ -1 +1 @@\n-old\n+new";
    let ct = detect_content_native(content);
    assert_eq!(ct, ContentType::GitDiff);
}

#[test]
fn detect_content_native_envelope() {
    let content = "<output>\n[1, 2, 3]\n</output>";
    let ct = detect_content_native(content);
    assert_eq!(ct, ContentType::JsonArray);
}

// --- strategy_from_detection ---

#[test]
fn strategy_from_detection_json() {
    assert_eq!(
        strategy_from_detection(ContentType::JsonArray, false),
        CompressionStrategy::SmartCrusher
    );
}

#[test]
fn strategy_from_detection_code() {
    assert_eq!(
        strategy_from_detection(ContentType::SourceCode, true),
        CompressionStrategy::CodeAware
    );
}

#[test]
fn strategy_from_detection_search() {
    assert_eq!(
        strategy_from_detection(ContentType::SearchResults, false),
        CompressionStrategy::Search
    );
}

#[test]
fn strategy_from_detection_log() {
    assert_eq!(
        strategy_from_detection(ContentType::BuildOutput, false),
        CompressionStrategy::Log
    );
}

#[test]
fn strategy_from_detection_diff() {
    assert_eq!(
        strategy_from_detection(ContentType::GitDiff, true),
        CompressionStrategy::Diff
    );
}

#[test]
fn strategy_from_detection_html() {
    assert_eq!(
        strategy_from_detection(ContentType::Html, true),
        CompressionStrategy::Html
    );
}

#[test]
fn strategy_from_detection_text() {
    assert_eq!(
        strategy_from_detection(ContentType::PlainText, true),
        CompressionStrategy::Kompress
    );
}

// --- CompressionStrategy ---

#[test]
fn compression_strategy_from_str() {
    assert_eq!(
        CompressionStrategy::from_str("smart_crusher"),
        Some(CompressionStrategy::SmartCrusher)
    );
    assert_eq!(
        CompressionStrategy::from_str("search"),
        Some(CompressionStrategy::Search)
    );
    assert_eq!(CompressionStrategy::from_str("unknown"), None);
}

// --- apply_strategy ---

#[test]
fn apply_strategy_smart_crusher() {
    let config = ContentRouterConfig::default();
    let content =
        r#"[{"id": 1, "name": "Alice"}, {"id": 2, "name": "Bob"}, {"id": 3, "name": "Charlie"}]"#;
    let (compressed, tokens, chain) = apply_strategy(
        content,
        CompressionStrategy::SmartCrusher,
        &config,
        "",
        None,
        1.0,
    );
    assert!(!compressed.is_empty());
    assert!(tokens <= 15); // SmartCrusher should compress
    assert_eq!(chain, vec!["smart_crusher"]);
}

#[test]
fn apply_strategy_log() {
    let config = ContentRouterConfig::default();
    let content = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10";
    let (compressed, _tokens, chain) =
        apply_strategy(content, CompressionStrategy::Log, &config, "", None, 1.0);
    assert!(!compressed.is_empty());
    assert_eq!(chain, vec!["log"]);
}

#[test]
fn apply_strategy_search() {
    // Two files, one match each, sharing a parent directory: the FILE fold
    // saves nothing here, but `search_dir_heading` factors out `src/`. The
    // STAGE 0 lossless fold therefore wins and the chain reports
    // `lossless_search` instead of reaching the lossy search compressor.
    // Byte-for-byte what Python's `compact_lossless(content, "search")`
    // returns for this input.
    let config = ContentRouterConfig::default();
    let content = "src/a.py:42: code\nsrc/b.py:10: other";
    let (compressed, _tokens, chain) =
        apply_strategy(content, CompressionStrategy::Search, &config, "", None, 1.0);
    assert_eq!(compressed, "src/\na.py:42: code\nb.py:10: other");
    assert_eq!(chain, vec!["lossless_search"]);
}

#[test]
fn apply_strategy_diff() {
    // STAGE 0 lossless-first (60af15f9): the `index` bookkeeping line folds
    // away byte-exact, so a diff whose fold shrinks returns `lossless_diff`
    // rather than reaching the lossy DiffCompressor.
    let config = ContentRouterConfig::default();
    let content =
        "diff --git a/f.py b/f.py\nindex 123..456\n--- a/f.py\n+++ b/f.py\n@@ -1 +1 @@\n-old\n+new";
    let (compressed, _tokens, chain) =
        apply_strategy(content, CompressionStrategy::Diff, &config, "", None, 1.0);
    assert!(!compressed.is_empty());
    assert_eq!(chain, vec!["lossless_diff"]);
}

#[test]
fn apply_strategy_passthrough() {
    let config = ContentRouterConfig::default();
    let content = "hello world";
    let (compressed, tokens, chain) = apply_strategy(
        content,
        CompressionStrategy::Passthrough,
        &config,
        "",
        None,
        1.0,
    );
    assert_eq!(compressed, content);
    assert_eq!(tokens, 2);
    assert_eq!(chain, vec!["passthrough"]);
}

#[test]
fn apply_strategy_disabled_compressor() {
    let config = config_with(|c| c.enable_smart_crusher = false);
    let content = "[1, 2, 3]";
    let (compressed, _tokens, chain) = apply_strategy(
        content,
        CompressionStrategy::SmartCrusher,
        &config,
        "",
        None,
        1.0,
    );
    assert_eq!(compressed, content);
    assert_eq!(chain, vec!["passthrough"]);
}

#[test]
fn apply_strategy_smart_crusher_fallback_to_log() {
    // Skip Kompress to test Log fallback directly. Non-JSON content —
    // SmartCrusher passes through unchanged, then Kompress is skipped
    // (disabled), then Log is attempted.
    let config = config_with(|c| {
        c.enable_log_compressor = true;
        c.enable_kompress = false;
    });
    let content = "unique one-off error that cannot be deduplicated or compressed";
    let original_tokens = content.split_whitespace().count();
    let (compressed, tokens, chain) = apply_strategy(
        content,
        CompressionStrategy::SmartCrusher,
        &config,
        "",
        None,
        1.0,
    );
    assert!(!compressed.is_empty());
    // SmartCrusher can't compress non-JSON, so fallback fires
    assert!(tokens >= original_tokens);
    // Chain records all attempted strategies even when they don't help
    assert_eq!(chain, vec!["smart_crusher", "kompress", "log"]);
}

// --- segment ---

#[test]
fn segment_empty() {
    assert_eq!(segment("", 8, 1200), Vec::<String>::new());
}

#[test]
fn segment_single_line() {
    assert_eq!(segment("hello", 8, 1200), vec!["hello"]);
}

#[test]
fn segment_blank_line_delimited() {
    let content = "line1\nline2\n\nline3\nline4";
    let segs = segment(content, 8, 1200);
    assert_eq!(segs.len(), 2);
    // Segments include trailing newlines from the split
    assert!(segs[0].contains("line1"));
    assert!(segs[0].contains("line2"));
    assert!(segs[1].contains("line3"));
    assert!(segs[1].contains("line4"));
}

#[test]
fn segment_dense_block() {
    let lines: Vec<String> = (0..20).map(|i| format!("line{}", i)).collect();
    let content = lines.join("\n");
    let segs = segment(&content, 8, 1200);
    assert!(segs.len() >= 3); // 20 lines / 8 per window = 3 windows
    // Verify lossless
    let rejoined = segs.join("\n");
    assert_eq!(rejoined, content);
}

#[test]
fn segment_long_line() {
    let long_line = "x".repeat(2000);
    let segs = segment(&long_line, 8, 1200);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0], long_line);
}

// --- otsu_threshold ---

#[test]
fn otsu_threshold_bimodal() {
    // Two clear groups: [0.1, 0.2, 0.3] and [0.8, 0.9, 1.0]
    let values = vec![0.1, 0.2, 0.3, 0.8, 0.9, 1.0];
    let t = otsu_threshold(&values);
    assert!(
        t > 0.3 && t < 0.8,
        "threshold should be between groups: {}",
        t
    );
}

#[test]
fn otsu_threshold_uniform() {
    let values = vec![0.5, 0.5, 0.5];
    let t = otsu_threshold(&values);
    assert_eq!(t, 0.5);
}

#[test]
fn otsu_threshold_empty() {
    assert_eq!(otsu_threshold(&[]), 0.0);
}

// --- adaptive_threshold ---

#[test]
fn adaptive_threshold_with_floor() {
    let values = vec![0.1, 0.2, 0.3, 0.8, 0.9, 1.0];
    let t = adaptive_threshold(&values, 0.5);
    assert!(t >= 0.5, "threshold should be at least floor: {}", t);
}

#[test]
fn adaptive_threshold_uniform_returns_floor() {
    let values = vec![0.5, 0.5, 0.5];
    let t = adaptive_threshold(&values, 0.3);
    assert_eq!(t, 0.3);
}

// --- plan_relevance_split ---

#[test]
fn plan_relevance_split_empty_query() {
    let content = "line1\n\nline2\n\nline3";
    let runs = plan_relevance_split(content, "", &[0.5, 0.3, 0.8], 0.3, true, None);
    assert_eq!(runs.len(), 1);
    assert!(runs[0].keep);
}

#[test]
fn plan_relevance_split_high_scores() {
    // Need enough lines to trigger segmentation (window=8, max_chars=1200)
    let content = "line1\nline2\n\nline3\nline4\n\nline5\nline6\n\nline7\nline8";
    let segs = segment(content, 8, 1200);
    // Should have 4 segments (blank-line delimited)
    assert_eq!(segs.len(), 4);
    // All scores above threshold -> all kept -> merged into 1 run
    let scores: Vec<f64> = segs.iter().map(|_| 0.9).collect();
    let runs = plan_relevance_split(content, "query", &scores, 0.3, true, None);
    assert_eq!(runs.len(), 1);
    assert!(runs[0].keep);
}

#[test]
fn plan_relevance_split_mixed_scores() {
    let content = "line1\n\nline2\n\nline3\n\nline4";
    let runs = plan_relevance_split(content, "query", &[0.9, 0.1, 0.9, 0.1], 0.5, false, None);
    // Should have alternating keep/drop runs
    assert!(runs.len() >= 2);
    assert!(runs[0].keep);
    assert!(!runs[1].keep);
}

#[test]
fn plan_relevance_split_single_segment() {
    let content = "just one line";
    let runs = plan_relevance_split(content, "query", &[0.5], 0.3, true, None);
    assert_eq!(runs.len(), 1);
    assert!(runs[0].keep);
}

#[test]
fn plan_relevance_split_max_records() {
    let lines: Vec<String> = (0..20).map(|i| format!("line{}\n", i)).collect();
    let content = lines.join("\n");
    let scores: Vec<f64> = (0..20)
        .map(|i| if i % 2 == 0 { 0.9 } else { 0.1 })
        .collect();
    let runs = plan_relevance_split(&content, "query", &scores, 0.5, true, Some(5));
    // Should be single keep run because segments > max_records
    assert_eq!(runs.len(), 1);
    assert!(runs[0].keep);
}

// ─── Savings profile tests ──────────────────────────────────────────

#[test]
fn savings_profile_agent90_settings() {
    let mut config = ContentRouterConfig::default();
    SavingsProfile::Agent90.apply_to(&mut config);
    assert_eq!(config.target_ratio, Some(0.10));
    assert_eq!(config.compress_user_messages, Some(true));
    assert_eq!(config.compress_system_messages, Some(true));
    assert_eq!(config.protect_recent_code, 2);
    assert!(config.force_kompress_all);
}

#[test]
fn savings_profile_balanced_settings() {
    let mut config = ContentRouterConfig::default();
    SavingsProfile::Balanced.apply_to(&mut config);
    assert_eq!(config.target_ratio, Some(0.30));
    assert_eq!(config.compress_user_messages, Some(false));
    assert_eq!(config.compress_system_messages, Some(false));
    assert_eq!(config.protect_recent_code, 4);
    assert!(!config.force_kompress_all);
}

#[test]
fn savings_profile_coding_settings() {
    let mut config = ContentRouterConfig::default();
    SavingsProfile::Coding.apply_to(&mut config);
    assert_eq!(config.target_ratio, None);
    assert_eq!(config.compress_user_messages, Some(false));
    assert_eq!(config.protect_recent_code, 2);
}

#[test]
fn savings_profile_general_settings() {
    let mut config = ContentRouterConfig::default();
    SavingsProfile::General.apply_to(&mut config);
    assert_eq!(config.target_ratio, None);
    assert_eq!(config.protect_recent_code, 0);
}

#[test]
fn savings_profile_from_str_roundtrip() {
    assert_eq!(
        SavingsProfile::from_str("agent-90"),
        Some(SavingsProfile::Agent90)
    );
    assert_eq!(
        SavingsProfile::from_str("balanced"),
        Some(SavingsProfile::Balanced)
    );
    assert_eq!(
        SavingsProfile::from_str("coding"),
        Some(SavingsProfile::Coding)
    );
    assert_eq!(
        SavingsProfile::from_str("general"),
        Some(SavingsProfile::General)
    );
    assert_eq!(SavingsProfile::from_str("unknown"), None);
}

#[test]
fn savings_profile_serde_roundtrip() {
    let json = serde_json::to_string(&SavingsProfile::Balanced).unwrap();
    assert_eq!(json, "\"balanced\"");
    let back: SavingsProfile = serde_json::from_str(&json).unwrap();
    assert_eq!(back, SavingsProfile::Balanced);
}

#[test]
fn target_ratio_in_config() {
    let mut config = ContentRouterConfig::default();
    assert_eq!(config.target_ratio, None);
    config.target_ratio = Some(0.25);
    assert_eq!(config.target_ratio, Some(0.25));
}

#[test]
fn per_provider_kompress_disable() {
    let mut config = ContentRouterConfig::default();
    config
        .disable_kompress_per_provider
        .insert("anthropic".to_string(), true);
    config
        .disable_kompress_per_provider
        .insert("openai".to_string(), false);
    assert!(config.disable_kompress_per_provider["anthropic"]);
    assert!(!config.disable_kompress_per_provider["openai"]);
}

#[test]
fn compress_user_system_messages_in_config() {
    let mut config = ContentRouterConfig::default();
    assert_eq!(config.compress_user_messages, None);
    assert_eq!(config.compress_system_messages, None);
    config.compress_user_messages = Some(true);
    config.compress_system_messages = Some(false);
    assert_eq!(config.compress_user_messages, Some(true));
    assert_eq!(config.compress_system_messages, Some(false));
}

// --- apply_strategy: untested paths ---

#[test]
fn apply_strategy_code_aware_compresses_code() {
    // Don't fall back to Kompress
    let config = config_with(|c| {
        c.enable_code_aware = true;
        c.enable_kompress = false;
    });
    let content = "function hello() {\n  console.log('world');\n  return 42;\n}";
    let (compressed, _tokens, chain) = apply_strategy(
        content,
        CompressionStrategy::CodeAware,
        &config,
        "",
        Some("javascript"),
        1.0,
    );
    assert!(!compressed.is_empty());
    assert_eq!(chain, vec!["code_aware"]);
}

#[test]
fn apply_strategy_code_aware_disabled_falls_through() {
    let config = config_with(|c| c.enable_code_aware = false);
    let content = "function hello() { return 42; }";
    let (compressed, _tokens, chain) = apply_strategy(
        content,
        CompressionStrategy::CodeAware,
        &config,
        "",
        Some("javascript"),
        1.0,
    );
    // Disabled → falls through to _ arm → passthrough
    assert_eq!(compressed, content);
    assert_eq!(chain, vec!["passthrough"]);
}

#[test]
fn apply_strategy_html_extracts_content() {
    let config = ContentRouterConfig::default();
    let content = "<!DOCTYPE html>\n<html>\n<head>\n<title>Test</title>\n<script>var analytics = {track: true}; function init() { console.log('boot'); }</script>\n<style>nav { background: #333; } footer { color: #666; }</style>\n</head>\n<body>\n<nav><a href=\"/\">Home</a> <a href=\"/about\">About</a></nav>\n<article>\n<h1>Main Heading</h1>\n<p>This is the first paragraph of actual article content with details.</p>\n<p>This is the second paragraph carrying even more meaningful details.</p>\n</article>\n<footer>Copyright 2024 | Privacy Policy | Terms of Service</footer>\n</body>\n</html>";
    let (compressed, tokens, chain) =
        apply_strategy(content, CompressionStrategy::Html, &config, "", None, 1.0);
    // Extraction wins: main content survives, boilerplate is stripped
    assert_eq!(chain, vec!["html_extractor"]);
    assert!(compressed.contains("first paragraph"));
    assert!(!compressed.contains("analytics"));
    assert!(tokens < content.split_whitespace().count());
}

#[test]
fn apply_strategy_html_disabled_falls_through() {
    let config = config_with(|c| c.enable_html_extractor = false);
    let content = "<html><body><h1>Hello</h1></body></html>";
    let (compressed, _tokens, chain) =
        apply_strategy(content, CompressionStrategy::Html, &config, "", None, 1.0);
    // Disabled → falls through to _ arm → passthrough
    assert_eq!(compressed, content);
    assert_eq!(chain, vec!["passthrough"]);
}

#[test]
fn apply_strategy_html_inflation_guard() {
    let config = ContentRouterConfig::default();
    // Tiny fragment: extraction yields nothing (or nothing smaller) →
    // original content is kept, chain records the attempt
    let content = "<html><body></body></html>";
    let (compressed, _tokens, chain) =
        apply_strategy(content, CompressionStrategy::Html, &config, "", None, 1.0);
    assert_eq!(compressed, content);
    assert_eq!(chain, vec!["html_extractor", "passthrough"]);
}

#[test]
fn apply_strategy_tabular_disabled_falls_through() {
    let config = config_with(|c| c.enable_kompress = false);
    let content = "col1,col2,col3\n1,2,3\n4,5,6";
    let (compressed, _tokens, chain) = apply_strategy(
        content,
        CompressionStrategy::Kompress,
        &config,
        "",
        None,
        1.0,
    );
    // Kompress disabled → falls through to _ (passthrough)
    assert_eq!(compressed, content);
    assert_eq!(chain, vec!["passthrough"]);
}

#[test]
fn apply_strategy_text_disabled_falls_through() {
    let config = config_with(|c| c.enable_kompress = false);
    let content = "just some plain text content here";
    let (compressed, _tokens, chain) =
        apply_strategy(content, CompressionStrategy::Text, &config, "", None, 1.0);
    // Text maps to Kompress which is disabled → passthrough
    assert_eq!(compressed, content);
    assert_eq!(chain, vec!["passthrough"]);
}

// --- STAGE 0 lossless-first dispatch (parity: 60af15f9) ---

fn grep_block() -> String {
    // Long, repeated path prefixes → search --heading fold collapses bytes
    // while word count stays flat/rises (heading re-emits path words).
    let paths = [
        "src/services/wallet/overdraft/automated_overdraft_initiation.py",
        "src/services/wallet/overdraft/capacity_limits.py",
    ];
    let mut lines = Vec::new();
    for p in paths {
        for ln in 1..40 {
            lines.push(format!(
                "{p}:{ln}:    result = compute_overdraft_capacity(business_id, amount)"
            ));
        }
    }
    format!("{}\n", lines.join("\n"))
}

#[test]
fn stage0_search_folds_lossless_byte_exact() {
    let block = grep_block();
    let config = ContentRouterConfig::default();
    let (out, _tokens, chain) =
        apply_strategy(&block, CompressionStrategy::Search, &config, "", None, 1.0);
    assert_eq!(chain, vec!["lossless_search"]);
    assert!(out.len() < block.len());
    // Word count is flat/higher — the byte-anchored fold still wins.
    assert!(out.split_whitespace().count() >= block.split_whitespace().count());
}

#[test]
fn lossless_only_mode_leaves_non_foldable_verbatim() {
    // Source code has no byte-lossless fold; in lossless-only mode it must be
    // left verbatim (passthrough), not lossy-dropped.
    let code = "fn main() {\n    println!(\"hi\");\n}\n";
    let config = config_with(|c| c.lossless = true);
    let (out, _t, chain) =
        apply_strategy(code, CompressionStrategy::CodeAware, &config, "", None, 1.0);
    assert_eq!(out, code);
    assert_eq!(chain, vec!["passthrough"]);
}

#[test]
fn lossless_only_mode_folds_search() {
    let block = grep_block();
    let config = config_with(|c| c.lossless = true);
    let (out, _t, chain) =
        apply_strategy(&block, CompressionStrategy::Search, &config, "", None, 1.0);
    assert_eq!(chain, vec!["lossless_search"]);
    assert!(out.len() < block.len());
}

#[test]
fn looks_like_diff_detects_unified_and_git() {
    assert!(looks_like_diff("diff --git a/x b/x\n"));
    assert!(looks_like_diff("--- a/x\n+++ b/x\n"));
    assert!(looks_like_diff("@@ -1,2 +1,2 @@\n"));
    assert!(looks_like_diff("foo\n@@ -1 +1 @@\n"));
    assert!(!looks_like_diff("just some text\nwith @@ inline"));
}

// ─── Frozen block verdicts (upstream addition) ───────────────────────

/// The env flag is process-global, so these tests share one lock rather
/// than racing each other under the default multi-threaded test runner.
fn freeze_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct FreezeFlag(Option<String>);
impl FreezeFlag {
    fn on() -> Self {
        let prev = std::env::var("HEADROOM_FREEZE_BLOCK_DECISION").ok();
        unsafe { std::env::set_var("HEADROOM_FREEZE_BLOCK_DECISION", "1") };
        Self(prev)
    }
}
impl Drop for FreezeFlag {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => unsafe { std::env::set_var("HEADROOM_FREEZE_BLOCK_DECISION", v) },
            None => unsafe { std::env::remove_var("HEADROOM_FREEZE_BLOCK_DECISION") },
        }
    }
}

#[test]
fn frozen_verdict_store_round_trips_and_clears() {
    let cache = CompressionCache::new(1800);
    assert_eq!(cache.frozen_verdict(7), None);
    cache.record_frozen_verdict(7, true);
    cache.record_frozen_verdict(8, false);
    assert_eq!(cache.frozen_verdict(7), Some(true));
    assert_eq!(cache.frozen_verdict(8), Some(false));
    // Clearing the cache must drop verdicts: they describe entries that no
    // longer exist.
    cache.clear();
    assert_eq!(cache.frozen_verdict(7), None);
}

#[test]
fn frozen_verdicts_evict_oldest_first_and_stay_bounded() {
    let cache = CompressionCache::new(1800);
    for k in 0..(FROZEN_VERDICTS_MAX as i64 + 10) {
        cache.record_frozen_verdict(k, true);
    }
    // Oldest evicted, newest retained, size capped.
    assert_eq!(cache.frozen_verdict(0), None);
    assert_eq!(
        cache.frozen_verdict(FROZEN_VERDICTS_MAX as i64 + 9),
        Some(true)
    );
    assert_eq!(
        cache.frozen.lock().unwrap().verdicts.len(),
        FROZEN_VERDICTS_MAX
    );
}

#[test]
fn re_recording_a_key_does_not_duplicate_its_eviction_slot() {
    let cache = CompressionCache::new(1800);
    cache.record_frozen_verdict(1, true);
    cache.record_frozen_verdict(1, false);
    let frozen = cache.frozen.lock().unwrap();
    assert_eq!(frozen.verdicts.len(), 1);
    assert_eq!(frozen.order.len(), 1, "order must not gain a second entry");
    assert!(!frozen.verdicts[&1], "later verdict wins");
}

#[test]
fn frozen_compress_verdict_pins_the_accept_threshold() {
    let _guard = freeze_env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _flag = FreezeFlag::on();
    let cache = CompressionCache::new(1800);
    // Unfrozen: the live per-turn gate applies.
    assert_eq!(cache.accept_threshold(1, 0.6), 0.6);
    // Frozen "compress": pinned to 1.0 so a tightened min_ratio can never
    // downgrade the block and bust the provider prefix cache.
    cache.record_frozen_verdict(1, true);
    assert_eq!(cache.accept_threshold(1, 0.6), 1.0);
    // A frozen "skip" is not a pin — it never warms the result cache.
    cache.record_frozen_verdict(2, false);
    assert_eq!(cache.accept_threshold(2, 0.6), 0.6);
}

#[test]
fn freeze_is_inert_while_the_flag_is_off() {
    let _guard = freeze_env_lock().lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::remove_var("HEADROOM_FREEZE_BLOCK_DECISION") };
    let cache = CompressionCache::new(1800);
    cache.record_frozen_verdict(1, true);
    assert_eq!(
        cache.accept_threshold(1, 0.6),
        0.6,
        "flag off must be byte-identical to the unfrozen path"
    );
}

#[test]
fn unrecoverable_lossy_compressions_are_never_frozen() {
    // #1307: pinning a lossy-unmarked strategy with no CCR marker would
    // keep serving a summary the agent cannot restore.
    for s in ["kompress", "text", "code_aware"] {
        assert!(
            !frozen_verdict_recoverable(s, Some("a fabricated summary")),
            "{s} without a marker must not be frozen"
        );
        assert!(
            frozen_verdict_recoverable(s, Some("summary <<ccr:abc123def456>>")),
            "{s} WITH a marker is recoverable"
        );
    }
    // Non-lossy strategies are always safe to pin.
    assert!(frozen_verdict_recoverable("lossless_search", Some("x")));
    assert!(frozen_verdict_recoverable("search", None));
}

#[test]
fn freeze_pin_accumulates_preserved_chars() {
    let cache = CompressionCache::new(1800);
    assert_eq!(cache.freeze_pin_stats(), (0, 0));
    cache.record_freeze_pin(&"x".repeat(100), 0.4);
    let (pins, chars) = cache.freeze_pin_stats();
    assert_eq!(pins, 1);
    assert_eq!(chars, 60, "100 chars at ratio 0.4 preserves ~60");
    // A ratio above 1.0 must not underflow the unsigned counter.
    cache.record_freeze_pin("short", 1.5);
    assert_eq!(cache.freeze_pin_stats().1, 60);
}

#[test]
fn lossless_first_no_fold_returns_none() {
    let (out, label) = lossless_first("short text", CompressionStrategy::Kompress);
    assert_eq!(out, "short text");
    assert!(label.is_none());
}

#[test]
fn lossless_first_never_diff_folds_non_diff_content() {
    // `diff_strip_index` is purely subtractive with no inverse check: it
    // deletes any `index <hex>..<hex>` line. On non-diff content that is
    // unrecoverable data loss dressed up as a lossless fold. The fold order
    // must therefore exclude "diff" unless the content really is a diff.
    let text = "Build log follows\nindex 1a2b3c4..5d6e7f8 100644\nrestore from index 1a2b3c4..5d6e7f8 100644\nall done\n";
    for strategy in [
        CompressionStrategy::Log,
        CompressionStrategy::Text,
        CompressionStrategy::Kompress,
        CompressionStrategy::Search,
    ] {
        let (out, label) = lossless_first(text, strategy);
        assert!(
            out.contains("index 1a2b3c4..5d6e7f8"),
            "the index line was silently dropped under {strategy:?} (label {label:?}): {out}"
        );
    }
}

#[test]
fn lossless_first_still_diff_folds_real_diffs() {
    // The guard must not cost us the fold on genuine diff content.
    let diff = "diff --git a/x b/x\nindex 1a2b3c4..5d6e7f8 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
    let (out, label) = lossless_first(diff, CompressionStrategy::Diff);
    assert!(
        !out.contains("index 1a2b3c4"),
        "diff index line should fold"
    );
    assert_eq!(label.as_deref(), Some("lossless_diff"));
    // And also when the strategy is wrong but the content sniffs as a diff.
    let (out2, _) = lossless_first(diff, CompressionStrategy::Text);
    assert!(!out2.contains("index 1a2b3c4"));
}

#[test]
fn lossless_first_can_choose_the_new_config_and_paths_folds() {
    let conf = "key: 1\nkey: 1\nkey: 1\n  - name: a\n    image: repo/svc:1\n    port: 8080\n    mem: 512Mi\n  - name: b\n    image: repo/svc:1\n    port: 8080\n    mem: 512Mi\n";
    let (out, label) = lossless_first(conf, CompressionStrategy::Config);
    assert!(out.len() < conf.len(), "config fold should shrink");
    assert_eq!(label.as_deref(), Some("lossless_config"));

    let paths = "src/handlers/alpha.rs\nsrc/handlers/beta.rs\nsrc/handlers/gamma.rs\n";
    let (out2, label2) = lossless_first(paths, CompressionStrategy::Text);
    assert!(out2.len() < paths.len(), "paths fold should shrink");
    assert_eq!(label2.as_deref(), Some("lossless_paths"));
}
