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
pub mod detection;
pub mod diff_compressor;
pub mod html_extractor;
#[cfg(feature = "ml")]
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
    detect_language, CodeAwareCompressor, CodeCompressionResult, CodeCompressorConfig,
    CodeLanguage, DocstringMode,
};
pub use content_detector::{
    detect_content_type, is_json_array_of_dicts, ContentType, DetectionResult,
};
pub use cross_turn_dedup::{
    dedup_blocks, dedup_blocks_with, dedup_messages, is_prefix_monotonic, DedupBlock, DedupStats,
};
pub use detection::detect;
pub use diff_compressor::{
    DiffCompressionResult, DiffCompressor, DiffCompressorConfig, DiffCompressorStats,
};
pub use html_extractor::{
    is_html_content, HtmlExtractionResult, HtmlExtractor, HtmlExtractorConfig,
};
#[cfg(feature = "ml")]
pub use kompress::{
    Kompress, KompressConfig, KompressError, KompressResult, DEFAULT_MODEL_ID,
    DEFAULT_TOKENIZER_REPO,
};
pub use live_zone::{
    compress_anthropic_all_messages, compress_anthropic_live_zone, compress_block_for_offload,
    compress_openai_chat_live_zone, compress_openai_responses_live_zone, set_kompress_enabled,
    summarize_openai_responses_no_change_reason, warm_live_zone_compressors, AuthMode, BlockAction,
    BlockOutcome, CompressionManifest, ExclusionReason, LiveZoneError, LiveZoneOutcome,
    DEFAULT_MODEL,
};
pub use log_compressor::{
    LogCompressionResult, LogCompressor, LogCompressorConfig, LogCompressorStats, LogFormat,
    LogLevel, LogLine,
};
#[cfg(feature = "ml")]
pub use magika_detector::{magika_detect, map_magika_label, MagikaDetectorError};
pub use pipeline::{
    CompressionContext, CompressionPipeline, CompressionPipelineBuilder, DiffNoise, DiffOffload,
    JsonMinifier, JsonOffload, LogOffload, LogTemplate, OffloadOutput, OffloadTransform,
    PipelineConfig, PipelineResult, ProseFieldOffload, ReformatOutput, ReformatTransform,
    TransformError,
};
pub use read_lifecycle::{
    format_read_lifecycle_transform, ReadClassification, ReadLifecycleConfig, ReadLifecycleManager,
    ReadLifecycleResult, ReadState,
};
pub use read_maturation::{
    relocate_cache_breakpoint, MaturationResult, MaturedRead, ReadMaturationConfig,
    ReadMaturationManager,
};
pub use recommendations::{Recommendation, RecommendationStore, RECOMMENDATIONS_PATH_ENV_VAR};
pub use safety::{tool_pair_indices, ToolPair};
pub use search_compressor::{
    FileMatches, SearchCompressionResult, SearchCompressor, SearchCompressorConfig,
    SearchCompressorStats, SearchMatch,
};
pub use spreadsheet_ingest::{load_spreadsheet, SpreadsheetError};
pub use tag_protector::{is_known_html_tag, protect_tags, restore_tags, ProtectStats};
pub use text_crusher::{TextCrusher, TextCrusherConfig, TextCrusherResult};
pub use unidiff_detector::{detect_diff, is_diff};
