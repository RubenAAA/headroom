use super::*;
use crate::transforms::compressor_registry::CompressOutput;
use std::collections::BTreeMap;
use std::sync::Mutex as StdMutex;

/// What the stub should return, so each test can drive a single branch.
enum Behavior {
    /// Shrink the content to a fixed short string.
    Shrink,
    /// Return more bytes than it was given.
    Expand,
    /// Blank the block out entirely.
    Empty,
    /// Shrink, and also emit a recovery map entry.
    ShrinkWithRecoverable,
}

struct StubExternal {
    descriptor: CompressorDescriptor,
    behavior: Behavior,
}

impl StubExternal {
    fn arc(name: &str, content_types: &[&str], behavior: Behavior) -> Arc<dyn Compressor> {
        Arc::new(Self {
            descriptor: CompressorDescriptor {
                name: name.to_string(),
                content_types: content_types.iter().map(|s| s.to_string()).collect(),
                lossless: false,
                cost_tier: "fast".to_string(),
                recoverable: matches!(behavior, Behavior::ShrinkWithRecoverable),
            },
            behavior,
        })
    }
}

impl Compressor for StubExternal {
    fn descriptor(&self) -> &CompressorDescriptor {
        &self.descriptor
    }

    fn compress(&self, input: &CompressInput) -> CompressOutput {
        match self.behavior {
            Behavior::Shrink => CompressOutput {
                content: "SHRUNK".to_string(),
                ..Default::default()
            },
            Behavior::Expand => CompressOutput {
                content: format!("{}{}", input.content, "x".repeat(64)),
                ..Default::default()
            },
            Behavior::Empty => CompressOutput {
                content: "   ".to_string(),
                ..Default::default()
            },
            Behavior::ShrinkWithRecoverable => {
                let mut recoverable = BTreeMap::new();
                recoverable.insert("deadbeef".to_string(), input.content.clone());
                CompressOutput {
                    content: "SHRUNK".to_string(),
                    recoverable,
                    ..Default::default()
                }
            }
        }
    }
}

fn registry_with(compressor: Arc<dyn Compressor>) -> CompressorRegistry {
    let mut registry = CompressorRegistry::new();
    registry.register(compressor, false).unwrap();
    registry
}

fn selecting(names: &[&str]) -> ContentRouterConfig {
    ContentRouterConfig {
        active_external_compressors: names.iter().map(|s| s.to_string()).collect(),
        // Keep the built-in path cheap and deterministic for these tests.
        enable_kompress: false,
        ..Default::default()
    }
}

/// Long enough that a shrink to "SHRUNK" is unambiguous.
const SAMPLE: &str = "the quick brown fox jumps over the lazy dog again and again and again";

fn run(
    content: &str,
    config: &ContentRouterConfig,
    registry: &CompressorRegistry,
) -> (String, usize, Vec<String>) {
    apply_strategy_with_registry(
        content,
        CompressionStrategy::Text,
        config,
        "",
        None,
        0.5,
        None,
        registry,
        &|_, _, _| true,
    )
}

/// The safety property that makes this change inert by default: with no
/// selection, the result is byte-identical to the built-in-only path.
#[test]
fn no_selection_means_the_external_path_is_never_reached() {
    let registry = registry_with(StubExternal::arc("ext", &["text/plain"], Behavior::Shrink));
    let config = config_with(|c| c.enable_kompress = false);
    assert!(config.active_external_compressors.is_empty());

    let (with_registry, _, chain) = run(SAMPLE, &config, &registry);
    let (builtin, _, builtin_chain) =
        apply_strategy(SAMPLE, CompressionStrategy::Text, &config, "", None, 0.5);

    assert_eq!(with_registry, builtin);
    assert_eq!(chain, builtin_chain);
    assert!(!chain.iter().any(|c| c.starts_with("external:")));
}

#[test]
fn a_selected_compressor_handles_a_matching_content_type() {
    let registry = registry_with(StubExternal::arc("ext", &["text/plain"], Behavior::Shrink));
    let (out, tokens, chain) = run(SAMPLE, &selecting(&["ext"]), &registry);

    assert_eq!(out, "SHRUNK");
    assert_eq!(chain, vec!["external:ext".to_string()]);
    // Counted with the router's own estimator, not the compressor's
    // self-reported (and here deliberately zero) tokens_after.
    assert_eq!(tokens, 1);
}

#[test]
fn a_wildcard_content_type_matches_anything() {
    for declared in [vec!["*"], vec!["*/*"], vec!["text/*"]] {
        let registry = registry_with(StubExternal::arc("ext", &declared, Behavior::Shrink));
        let (out, _, chain) = run(SAMPLE, &selecting(&["ext"]), &registry);
        assert_eq!(
            out, "SHRUNK",
            "declared {declared:?} should match text/plain"
        );
        assert_eq!(chain, vec!["external:ext".to_string()]);
    }
}

#[test]
fn a_non_matching_content_type_falls_through_to_the_builtin() {
    let registry = registry_with(StubExternal::arc(
        "ext",
        &["application/json"],
        Behavior::Shrink,
    ));
    let (out, _, chain) = run(SAMPLE, &selecting(&["ext"]), &registry);

    assert_ne!(out, "SHRUNK");
    assert!(!chain.iter().any(|c| c.starts_with("external:")));
}

/// `image/*` must not match `text/plain` — the type wildcard is scoped to
/// its own top-level type, otherwise it would be a full wildcard.
#[test]
fn a_type_wildcard_does_not_cross_content_types() {
    let registry = registry_with(StubExternal::arc("ext", &["image/*"], Behavior::Shrink));
    let (out, _, chain) = run(SAMPLE, &selecting(&["ext"]), &registry);

    assert_ne!(out, "SHRUNK");
    assert!(!chain.iter().any(|c| c.starts_with("external:")));
}

#[test]
fn an_expanding_compressor_is_rejected() {
    let registry = registry_with(StubExternal::arc("ext", &["text/plain"], Behavior::Expand));
    let (out, _, chain) = run(SAMPLE, &selecting(&["ext"]), &registry);

    assert!(out.len() <= SAMPLE.len());
    assert!(!chain.iter().any(|c| c.starts_with("external:")));
}

/// Blanking a non-empty block makes providers reject the whole request, so
/// an empty result must fall back rather than be returned.
#[test]
fn a_compressor_that_blanks_the_block_is_rejected() {
    let registry = registry_with(StubExternal::arc("ext", &["text/plain"], Behavior::Empty));
    let (out, _, chain) = run(SAMPLE, &selecting(&["ext"]), &registry);

    assert!(!out.trim().is_empty());
    assert!(!chain.iter().any(|c| c.starts_with("external:")));
}

#[test]
fn selecting_an_unregistered_name_is_not_fatal() {
    let registry = registry_with(StubExternal::arc("ext", &["text/plain"], Behavior::Shrink));
    let (out, _, chain) = run(SAMPLE, &selecting(&["ghost"]), &registry);

    assert_ne!(out, "SHRUNK");
    assert!(!chain.iter().any(|c| c.starts_with("external:")));
}

#[test]
fn the_recovery_map_is_handed_to_the_store() {
    let registry = registry_with(StubExternal::arc(
        "ext",
        &["text/plain"],
        Behavior::ShrinkWithRecoverable,
    ));
    let stored: StdMutex<Vec<(String, String, String)>> = StdMutex::new(Vec::new());

    let (out, _, chain) = apply_strategy_with_registry(
        SAMPLE,
        CompressionStrategy::Text,
        &selecting(&["ext"]),
        "",
        None,
        0.5,
        None,
        &registry,
        &|hash, original, strategy| {
            stored.lock().unwrap().push((
                hash.to_string(),
                original.to_string(),
                strategy.to_string(),
            ));
            true
        },
    );

    assert_eq!(out, "SHRUNK");
    assert_eq!(chain, vec!["external:ext".to_string()]);
    let entries = stored.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "deadbeef");
    assert_eq!(entries[0].1, SAMPLE);
    assert_eq!(entries[0].2, "external:ext");
}

/// A store failure must not break the request — the compressed block is
/// still returned, only that entry is unretrievable.
#[test]
fn a_store_failure_still_returns_the_compressed_block() {
    let registry = registry_with(StubExternal::arc(
        "ext",
        &["text/plain"],
        Behavior::ShrinkWithRecoverable,
    ));

    let (out, _, chain) = apply_strategy_with_registry(
        SAMPLE,
        CompressionStrategy::Text,
        &selecting(&["ext"]),
        "",
        None,
        0.5,
        None,
        &registry,
        &|_, _, _| false,
    );

    assert_eq!(out, "SHRUNK");
    assert_eq!(chain, vec!["external:ext".to_string()]);
}

#[test]
fn strategy_to_mime_matches_python() {
    let cases = [
        (CompressionStrategy::CodeAware, "text/x-code"),
        (CompressionStrategy::SmartCrusher, "application/json"),
        (CompressionStrategy::Search, "text/x-search-results"),
        (CompressionStrategy::Log, "text/x-log"),
        (CompressionStrategy::Diff, "text/x-diff"),
        (CompressionStrategy::Html, "text/html"),
        (CompressionStrategy::Tabular, "text/csv"),
        (CompressionStrategy::Config, "text/x-config"),
        (CompressionStrategy::Text, "text/plain"),
        (CompressionStrategy::Kompress, "text/plain"),
        (CompressionStrategy::Passthrough, "text/plain"),
        // Unmapped in Python's dict → PLAIN_TEXT via `.get` default.
        (CompressionStrategy::Mixed, "text/plain"),
    ];
    for (strategy, expected) in cases {
        assert_eq!(
            content_type_mime(content_type_from_strategy(strategy)),
            expected,
            "{strategy:?}"
        );
    }
}

/// Lossless-only mode returns at STAGE 0, so an external compressor can
/// never inject unrecoverable loss into a lossless-only session.
#[test]
fn lossless_only_mode_never_reaches_the_external_path() {
    let registry = registry_with(StubExternal::arc("ext", &["*"], Behavior::Shrink));
    let config = ContentRouterConfig {
        lossless: true,
        ..selecting(&["ext"])
    };

    let (out, _, chain) = run(SAMPLE, &config, &registry);
    assert_ne!(out, "SHRUNK");
    assert!(!chain.iter().any(|c| c.starts_with("external:")));
}
