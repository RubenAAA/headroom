//! Compression transforms — Rust ports of `headroom.transforms.*`.
//!
//! # Guiding principle: information preservation > aggressive compression
//!
//! When in doubt, prefer keeping bytes. The fixtures lock the Python
//! algorithm's exact behavior, so this crate cannot drop information that
//! Python keeps. But the inverse is also true — we MUST drop everything
//! Python drops, even when it feels lossy. Stage 3a's faithful port is
//! parity-bound. A follow-up stage (token-budget-aware compression) is
//! where we earn the right to keep more.
//!
//! Observability is the escape hatch: every transform returns a sidecar
//! `Stats` struct with the granular metrics Python doesn't emit (e.g. which
//! files were dropped, how many context lines were trimmed, per-file hunk
//! drop counts). These flow through `tracing` spans for OTel scraping in
//! prod and are returned alongside the parity-equal output for tests.

pub mod adaptive_sizer;
pub mod anchor_selector;
pub mod base;
pub mod cache_aligner;
pub mod code_compressor;
pub mod cold_prefix;
pub mod compression_batches;
pub mod compression_summary;
pub mod compression_units;
pub mod compressor_registry;
pub mod config_compressor;
pub mod content_detector;
pub mod content_router;
pub mod cross_turn_dedup;
pub mod dense_line_elider;
pub mod detection;
pub mod diff_compressor;
pub mod html_extractor;
pub mod kompress;
pub mod kompress_remote;
pub mod live_zone;
pub mod log_compressor;
pub mod lossless_compaction;
#[cfg(feature = "ml")]
pub mod magika_detector;
pub mod observability;
pub mod pipeline;
pub mod read_lifecycle;
pub mod read_maturation;
pub mod read_protection;
pub mod recommendations;
pub mod relevance_split;
pub mod safety;
pub mod search_compressor;
pub mod smart_crusher;
pub mod spreadsheet_ingest;
pub mod tabular_ingest;
pub mod tag_protector;
pub mod text_crusher;
pub mod thinking_compactor;
pub mod unidiff_detector;

pub use cache_aligner::{
    CacheAligner, CacheAlignerConfig, CacheAlignerResult, CacheAlignerState, CachePrefixMetrics,
    VolatileFinding,
};
pub use code_compressor::{
    CodeAwareCompressor, CodeCompressionResult, CodeCompressorConfig, CodeLanguage, DocstringMode,
    SyntaxBreakerLanguageStatus, detect_language, syntax_breaker_status,
};
pub use content_detector::{
    ContentType, DetectionResult, detect_content_type, is_json_array_of_dicts,
};
pub use cross_turn_dedup::{
    DedupBlock, DedupStats, dedup_blocks, dedup_blocks_with, dedup_messages, is_prefix_monotonic,
};
pub use detection::detect;
pub use diff_compressor::{
    DiffCompressionResult, DiffCompressor, DiffCompressorConfig, DiffCompressorStats,
};
pub use html_extractor::{
    HtmlExtractionResult, HtmlExtractor, HtmlExtractorConfig, is_html_content,
};
/// The loaded ONNX model itself only exists with the `ml` feature; its config,
/// result and error types are plain data and are re-exported either way.
#[cfg(feature = "ml")]
pub use kompress::Kompress;
pub use kompress::{
    DEFAULT_MODEL_ID, DEFAULT_TOKENIZER_REPO, KompressConfig, KompressError, KompressResult,
};
pub use live_zone::{
    AuthMode, BlockAction, BlockOutcome, CompressionManifest, DEFAULT_MODEL, DispatchConfig,
    ExclusionReason, LiveZoneError, LiveZoneOutcome, code_aware_enabled,
    compress_anthropic_all_messages, compress_anthropic_live_zone,
    compress_anthropic_live_zone_with_ccr, compress_block_for_offload,
    compress_openai_chat_live_zone, compress_openai_chat_live_zone_with_config,
    compress_openai_responses_live_zone, compress_openai_responses_live_zone_with_config,
    set_code_aware_enabled, set_kompress_enabled, summarize_openai_responses_no_change_reason,
    warm_live_zone_compressors,
};
pub use log_compressor::{
    LogCompressionResult, LogCompressor, LogCompressorConfig, LogCompressorStats, LogFormat,
    LogLevel, LogLine,
};
#[cfg(feature = "ml")]
pub use magika_detector::{MagikaDetectorError, magika_detect, map_magika_label};
pub use pipeline::{
    CompressionContext, CompressionPipeline, CompressionPipelineBuilder, DiffNoise, DiffOffload,
    JsonMinifier, JsonOffload, LogOffload, LogTemplate, OffloadOutput, OffloadTransform,
    PipelineConfig, PipelineResult, ProseFieldOffload, ReformatOutput, ReformatTransform,
    TransformError,
};
pub use read_lifecycle::{
    ReadClassification, ReadLifecycleConfig, ReadLifecycleManager, ReadLifecycleResult, ReadState,
    format_read_lifecycle_transform,
};
pub use read_maturation::{
    MaturationResult, MaturedRead, ReadMaturationConfig, ReadMaturationManager,
    relocate_cache_breakpoint,
};
pub use recommendations::{RECOMMENDATIONS_PATH_ENV_VAR, Recommendation, RecommendationStore};
pub use safety::{ToolPair, tool_pair_indices};
pub use search_compressor::{
    FileMatches, SearchCompressionResult, SearchCompressor, SearchCompressorConfig,
    SearchCompressorStats, SearchMatch,
};
pub use spreadsheet_ingest::{SpreadsheetError, load_spreadsheet};
pub use tag_protector::{ProtectStats, is_known_html_tag, protect_tags, restore_tags};
pub use text_crusher::{TextCrusher, TextCrusherConfig, TextCrusherResult};
pub use unidiff_detector::{detect_diff, is_diff};
