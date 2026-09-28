use super::*;

/// `HEADROOM_KOMPRESS_MAX_TOKENS` is process-global, so these tests cannot
/// run concurrently with each other.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Set the ceiling for the duration of a test, restoring it afterwards.
struct CeilingGuard(Option<String>);

impl CeilingGuard {
    fn set(value: Option<&str>) -> Self {
        let prior = std::env::var("HEADROOM_KOMPRESS_MAX_TOKENS").ok();
        match value {
            Some(v) => unsafe { std::env::set_var("HEADROOM_KOMPRESS_MAX_TOKENS", v) },
            None => unsafe { std::env::remove_var("HEADROOM_KOMPRESS_MAX_TOKENS") },
        }
        Self(prior)
    }
}

impl Drop for CeilingGuard {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => unsafe { std::env::set_var("HEADROOM_KOMPRESS_MAX_TOKENS", v) },
            None => unsafe { std::env::remove_var("HEADROOM_KOMPRESS_MAX_TOKENS") },
        }
    }
}

#[test]
fn the_default_ceiling_matches_python() {
    let _lock = env_lock();
    let _guard = CeilingGuard::set(None);
    assert_eq!(kompress_max_tokens(), 50_000);
    assert_eq!(DEFAULT_KOMPRESS_MAX_TOKENS, 50_000);
}

/// An unparseable value falls back to the default, matching Python's
/// bare-except around `int(...)`.
#[test]
fn an_unparseable_ceiling_falls_back_to_the_default() {
    let _lock = env_lock();
    let _guard = CeilingGuard::set(Some("not-a-number"));
    assert_eq!(kompress_max_tokens(), DEFAULT_KOMPRESS_MAX_TOKENS);
}

/// The threshold is chars > tokens * 4, so it fires just past the boundary
/// and not at it.
#[test]
fn the_gate_fires_only_above_the_ceiling() {
    let _lock = env_lock();
    let _guard = CeilingGuard::set(Some("10"));

    assert!(
        !kompress_size_gate_exceeded(&"x".repeat(40)),
        "at the boundary"
    );
    assert!(kompress_size_gate_exceeded(&"x".repeat(41)), "one past it");
}

/// A zero ceiling disables the gate entirely, however large the block.
#[test]
fn a_zero_ceiling_disables_the_gate() {
    let _lock = env_lock();
    let _guard = CeilingGuard::set(Some("0"));
    assert!(!kompress_size_gate_exceeded(&"x".repeat(1_000_000)));
}

/// The point of the gate: an oversized block must come back through the
/// cheap path with the gate recorded in the chain, never reaching ML.
#[test]
fn an_oversized_block_is_routed_off_ml() {
    let _lock = env_lock();
    let _guard = CeilingGuard::set(Some("10"));

    let content = "the quick brown fox jumps over the lazy dog. ".repeat(40);
    let config = ContentRouterConfig::default();
    let (_out, _tokens, chain) = try_kompress(
        &content,
        &config,
        "",
        &["kompress".to_string()],
        &|_, _, _| true,
    );

    assert!(
        chain.contains(&"kompress_size_gate".to_string()),
        "chain should record the gate, got {chain:?}"
    );
}

/// Under the ceiling the gate must not appear in the chain at all.
#[test]
fn a_small_block_is_not_gated() {
    let _lock = env_lock();
    let _guard = CeilingGuard::set(Some("50000"));

    let config = ContentRouterConfig::default();
    let (_out, _tokens, chain) = try_kompress(
        "short text",
        &config,
        "",
        &["kompress".to_string()],
        &|_, _, _| true,
    );

    assert!(!chain.contains(&"kompress_size_gate".to_string()));
}

/// An image block used to be stringified, embedding its whole base64
/// payload in the count. S is the cache-bust cost, so that inflated S for
/// every message before it and the break-even gate refused to compress
/// any of them.
#[test]
fn image_blocks_are_not_counted_as_their_base64_payload() {
    let big_payload = "A".repeat(80_000);
    let with_image = serde_json::json!([
        {"type": "text", "text": "look at this"},
        {"type": "image", "source": {"type": "base64", "data": big_payload}},
    ]);
    let counted = netcost_message_tokens(&with_image);
    // text words + the flat image cost, nowhere near the payload size.
    assert!(
        counted < 2_000,
        "image priced at {counted} tokens; base64 payload leaked into the count"
    );
    assert!(counted >= crate::tokenizer::IMAGE_TOKENS);
}

#[test]
fn dense_elide_fires_on_passthrough_bundle_dump() {
    // A minified-bundle dump: no structural compressor understands it.
    // Elision must fire on the passthrough path with the strategy named.
    let config = config_with(|c| c.enable_kompress = false);
    // A minified-bundle dump with prose around it: no structural
    // compressor understands the dense lines, so elision fires on the
    // passthrough path with the strategy named.
    let dump = format!(
        "Script completed\n{}\nSome prose with plenty of spaces to separate the dumps.\n{}",
        "a".repeat(3000),
        "b".repeat(3000)
    );
    let (out, tokens, chain) =
        apply_strategy(&dump, CompressionStrategy::Text, &config, "", None, 1.0);
    assert!(chain.contains(&"dense_elide".to_string()), "{chain:?}");
    assert!(tokens < dump.len() / 4);
    assert!(out.contains("chars of dense machine-generated content elided"));
}

#[test]
fn dense_elide_runs_after_code_aware_and_on_a_script_only_page() {
    let config = config_with(|c| {
        c.enable_kompress = false;
        c.enable_code_aware = true;
    });
    let bundle = "x".repeat(4000);

    let code = format!(
        "def main():\n    blob = '{bundle}'\n    return blob\n\n\ndef helper(a, b):\n    return a + b\n"
    );
    let (out, _, chain) = apply_strategy(
        &code,
        CompressionStrategy::CodeAware,
        &config,
        "",
        Some("python"),
        1.0,
    );
    assert_eq!(chain.first().map(String::as_str), Some("code_aware"));
    assert!(chain.contains(&"dense_elide".to_string()), "{chain:?}");
    assert!(out.len() < code.len() / 2);

    // All <script>: extraction yields whitespace, which is "nothing
    // extracted", so the page falls through to dense elision instead of
    // passing through whole or being replaced by an empty block.
    let page = format!("<html><head><script>{bundle}</script></head>\n<body>\n  \n</body></html>");
    let (out, _, chain) = apply_strategy(&page, CompressionStrategy::Html, &config, "", None, 1.0);
    assert!(chain.contains(&"dense_elide".to_string()), "{chain:?}");
    assert!(!out.trim().is_empty());
    assert!(out.len() < page.len() / 2);
}

#[test]
fn dense_elide_stays_off_without_flag_or_in_lossless() {
    let dump = format!(
        "Script completed\n{}\nSome prose with plenty of spaces.\n{}",
        "a".repeat(3000),
        "b".repeat(3000)
    );
    let off = ContentRouterConfig {
        enable_dense_line_elision: false,
        ..Default::default()
    };
    let (_, _, chain) = apply_strategy(&dump, CompressionStrategy::Log, &off, "", None, 1.0);
    assert!(!chain.contains(&"dense_elide".to_string()));

    let lossless = ContentRouterConfig {
        lossless: true,
        ..Default::default()
    };
    let (_, _, chain) = apply_strategy(&dump, CompressionStrategy::Log, &lossless, "", None, 1.0);
    assert!(!chain.contains(&"dense_elide".to_string()));
}

#[test]
fn text_and_tool_result_counting_is_unchanged() {
    let blocks = serde_json::json!([
        {"type": "text", "text": "one two three"},
        {"type": "tool_result", "content": "four five"},
    ]);
    assert_eq!(netcost_message_tokens(&blocks), 5);
    assert_eq!(netcost_message_tokens(&serde_json::json!("a b c")), 3);
}
