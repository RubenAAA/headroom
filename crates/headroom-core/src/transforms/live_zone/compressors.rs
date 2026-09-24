//! Shared compressor instances, the code-aware and Kompress switches, and
//! startup warm-up.
//!
//! Moved out of `live_zone.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

// ─── Compressor singletons ─────────────────────────────────────────────
//
// Each compressor's struct holds its config + (for SmartCrusher) the
// scoring infrastructure. Allocating one per request would be
// wasteful and (in SmartCrusher's case) defeats the purpose of the
// builder. Hold one instance per process behind `OnceLock`; cheap to
// clone the &reference each call.

pub(super) fn smart_crusher() -> &'static SmartCrusher {
    static INSTANCE: OnceLock<SmartCrusher> = OnceLock::new();
    INSTANCE.get_or_init(|| SmartCrusher::new(SmartCrusherConfig::default()))
}

pub(super) fn log_compressor() -> &'static LogCompressor {
    static INSTANCE: OnceLock<LogCompressor> = OnceLock::new();
    INSTANCE.get_or_init(|| LogCompressor::new(LogCompressorConfig::default()))
}

pub(super) fn search_compressor() -> &'static SearchCompressor {
    static INSTANCE: OnceLock<SearchCompressor> = OnceLock::new();
    INSTANCE.get_or_init(|| SearchCompressor::new(SearchCompressorConfig::default()))
}

pub(super) fn diff_compressor() -> &'static DiffCompressor {
    static INSTANCE: OnceLock<DiffCompressor> = OnceLock::new();
    INSTANCE.get_or_init(|| DiffCompressor::new(DiffCompressorConfig::default()))
}

// CodeCompressor needs no model or network — the tree-sitter grammars are
// statically linked, so `OnceLock` construction is microseconds.
pub(super) fn code_compressor() -> &'static CodeAwareCompressor {
    static INSTANCE: OnceLock<CodeAwareCompressor> = OnceLock::new();
    INSTANCE.get_or_init(|| CodeAwareCompressor::new(CodeCompressorConfig::default()))
}

// Process-wide gate for the CodeAware (`SourceCode`) compressor. Default ON:
// this preserves the historical dispatch behavior (the arm predates the
// flag). The proxy sets this once at startup from `--code-aware`, so an
// operator that wants the off-arm gets it, while our deployment pins it on.
// Mirrors the `KOMPRESS_ENABLED` pattern below.
pub(super) static CODE_AWARE_ENABLED: AtomicBool = AtomicBool::new(true);

/// Enable or disable the CodeAware `SourceCode` compressor process-wide.
/// Call once at startup (before serving) from config. When disabled, source
/// code blocks pass through untouched.
pub fn set_code_aware_enabled(enabled: bool) {
    CODE_AWARE_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Current state of the CodeAware process-wide gate (default `true`).
pub fn code_aware_enabled() -> bool {
    CODE_AWARE_ENABLED.load(Ordering::Relaxed)
}

// Process-wide gate for the Kompress (PlainText) compressor. Default OFF:
// unlike the structural compressors, Kompress
// carries a ~261 MB ONNX model, so an operator must opt in before it is ever
// loaded. Mirrors the Python reference's `config.enable_kompress`. The proxy
// sets this once at startup from `--enable-kompress`.
pub(super) static KOMPRESS_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable or disable the Kompress `PlainText` compressor process-wide. Call
/// once at startup (before serving) from config. When disabled, plain-text
/// blocks pass through and the model is never loaded.
pub fn set_kompress_enabled(enabled: bool) {
    KOMPRESS_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Kompress readiness for health reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KompressStatus {
    /// Flag off: the model is never loaded, plain text passes through.
    Disabled,
    /// Flag on but no loaded model (still warming, uncached, or the cached
    /// load failed): plain text passes through. Soft state — never fails
    /// readiness (mirrors upstream excluding Kompress from overall `ready`).
    Deferred,
    /// Model cached and loaded: Kompress serves plain-text blocks.
    Loaded,
}

impl std::fmt::Display for KompressStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KompressStatus::Disabled => write!(f, "disabled"),
            KompressStatus::Deferred => write!(f, "deferred"),
            KompressStatus::Loaded => write!(f, "loaded"),
        }
    }
}

/// Non-loading, non-blocking Kompress status. Reads the same statics the
/// request path reads, so the answer matches serving behavior by
/// construction. Never triggers a load.
pub fn kompress_status() -> KompressStatus {
    if !KOMPRESS_ENABLED.load(Ordering::Relaxed) {
        return KompressStatus::Disabled;
    }
    #[cfg(feature = "ml")]
    match KOMPRESS_INSTANCE.get() {
        Some(Some(_)) => KompressStatus::Loaded,
        _ => KompressStatus::Deferred,
    }
    // Without `ml` there is no ONNX session to load, so the switch being on
    // changes nothing — report it the same way a switched-off model reads.
    #[cfg(not(feature = "ml"))]
    KompressStatus::Disabled
}

// Loaded Kompress singleton. Populated **only** by `warm_live_zone_compressors`
// (an off-request-path startup call) — never by `kompress()` on the request
// path. The model is a ~261 MB ONNX session whose load can be slow (and on the
// OpenVINO/NPU EP, the int8 graph compile can take many seconds), so the
// request path must never trigger or block on that init. See `kompress()`.
#[cfg(feature = "ml")]
pub(super) static KOMPRESS_INSTANCE: OnceLock<Option<Kompress>> = OnceLock::new();

// Kompress is the ML prose compressor. Unlike the others it carries a ~261 MB
// ONNX model, so: (1) it is gated behind `KOMPRESS_ENABLED` (a disabled proxy
// never loads the model), and (2) it loads **cache-only**, mirroring the Python
// reference's `allow_download=False` path.
//
// CRITICAL: this request-path accessor is **non-blocking**. It does a plain
// `OnceLock::get()` — if the model has not finished warming yet (or was never
// cached), it returns `None` and the dispatcher passes plain text through,
// exactly as when Kompress is unavailable. It must NOT call `get_or_init`:
// doing so makes a request thread block on the (possibly slow/hung) model load,
// which previously stalled the whole proxy when the NPU compile didn't return.
// The load happens once, off the request path, in `warm_live_zone_compressors`.
#[cfg(feature = "ml")]
pub(super) fn kompress() -> Option<&'static Kompress> {
    if !KOMPRESS_ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    // Non-blocking: `None` until `warm` has populated the slot.
    KOMPRESS_INSTANCE.get().and_then(|slot| slot.as_ref())
}

/// Eagerly initialize the live-zone compressor singletons off the request
/// path — the Rust mirror of the Python reference's `eager_load_compressors`.
///
/// This is the **only** place the Kompress model is loaded. Kompress is loaded
/// only when enabled via [`set_kompress_enabled`], and even then **cache-only**
/// (never downloads). The load can block (an NPU graph compile takes seconds),
/// so call this on a dedicated blocking/startup thread — never on the request
/// path. Until it completes, [`kompress`] returns `None` and `PlainText` blocks
/// pass through; once it completes the model is live for subsequent requests.
/// Returns whether the Kompress model was cached/loaded (`false` when disabled
/// or not cached). Idempotent: the underlying `OnceLock` loads at most once.
pub fn warm_live_zone_compressors() -> bool {
    // CodeCompressor: statically-linked grammars, trivial to construct.
    let _ = code_compressor();
    // Kompress: perform the (potentially slow) load here, off the request path.
    // `Some` iff enabled AND the model was already in the HF cache.
    #[cfg(feature = "ml")]
    {
        KOMPRESS_INSTANCE
            .get_or_init(|| {
                if !KOMPRESS_ENABLED.load(Ordering::Relaxed) {
                    return None;
                }
                Kompress::from_cache(KompressConfig::default())
                    .ok()
                    .flatten()
            })
            .is_some()
    }
    // No ONNX Runtime linked in, so there is nothing to warm and no model to
    // report as ready. The lexical compressors above are warmed either way.
    #[cfg(not(feature = "ml"))]
    false
}
