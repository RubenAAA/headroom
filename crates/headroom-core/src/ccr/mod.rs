//! CCR (Compress-Cache-Retrieve) storage layer.
//!
//! When a transform compresses data with row-drop or opaque-string
//! substitution, the *original payload* is stashed here keyed by the
//! hash that ends up in the prompt. The runtime later honors retrieval
//! tool calls by looking up the hash in this store and serving back the
//! original. This is the cornerstone of CCR: lossy on the wire, lossless
//! end-to-end.
//!
//! Mirrors the semantics of Python's [`CompressionStore`] (`headroom/
//! cache/compression_store.py`) but stripped down to the contract that
//! actually matters for retrieval — no BM25 search, no retrieval-event
//! feedback, no per-tool metadata. Those live in the runtime layer; this
//! crate only needs put/get.
//!
//! # Backends
//!
//! - [`backends::InMemoryCcrStore`] — process-local, sharded `DashMap`.
//!   Test default; lost on restart, fragmented across workers.
//! - [`backends::SqliteCcrStore`] — production default. Persistent
//!   across worker restarts; shareable across workers via a shared DB
//!   file. WAL-mode, prepared statements, lazy TTL purge on read.
//! - [`backends::RedisCcrStore`] — multi-worker opt-in (cfg-gated
//!   behind `feature = "redis"`). No sticky-session required at the
//!   load balancer.
//!
//! [`backends::from_config`] selects one at startup and surfaces every
//! init error to the caller (per `feedback_no_silent_fallbacks.md`).
//!
//! [`CompressionStore`]: https://github.com/headroomlabs-ai/headroom/blob/main/headroom/cache/compression_store.py

pub mod backends;
pub mod batch_processor;
pub mod batch_store;
pub mod context_tracker;
pub mod response_handler;
pub mod tool_injection;

use std::time::Duration;

pub use backends::{CcrBackendConfig, CcrBackendInitError, InMemoryCcrStore, from_config};
pub use batch_processor::{
    BatchResultProcessor, BatchResultProcessorConfig, ContinuationRequest, ProcessedBatchResult,
};
pub use batch_store::{BatchContext, BatchContextStore, BatchRequestContext, BatchStoreStats};
pub use response_handler::{
    CCRResponseHandler, CcrToolCall, CcrToolResult, ResponseHandlerConfig, ResponseHandlerStats,
    StreamingCcrBuffer, StreamingCcrHandler,
};

/// Pluggable CCR storage backend. `Send + Sync` so it can sit behind an
/// `Arc` and be shared across threads in the proxy.
pub trait CcrStore: Send + Sync {
    /// Stash `payload` under `hash`. If the hash already exists, the
    /// new payload overwrites — same hash should mean same content, so
    /// re-storing is idempotent. Returns `true` if the payload is now
    /// durably stored and retrievable via `get`, `false` if the write
    /// failed (backend-specific error, already logged by the impl) —
    /// callers that report "offloaded" metrics should gate on this.
    fn put(&self, hash: &str, payload: &str) -> bool;

    /// Look up `hash`. Returns `None` if missing or expired.
    fn get(&self, hash: &str) -> Option<String>;

    /// Number of live entries. Informational; used by tests + telemetry.
    /// Some backends (notably Redis) cannot answer this efficiently and
    /// return 0 — see backend-specific docs.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Store-wide idle-window TTL, for availability reporting. Backends
    /// already carry this as configuration; the default covers stores
    /// that cannot report one.
    fn default_ttl_seconds(&self) -> u64 {
        DEFAULT_TTL.as_secs()
    }

    /// Availability + TTL metadata for `hash`, mirroring Python
    /// `CompressionStore.get_entry_status(hash, clean_expired=True)`:
    /// expired entries are purged as a side effect. The default impl
    /// cannot distinguish expired from missing, so it reports `Missing`
    /// — backends with per-entry timestamps (memory, sqlite) override
    /// this; Redis overrides with an existence probe (present means
    /// available, since Redis evicts expired keys itself).
    fn entry_status(&self, hash: &str) -> CcrEntryStatus {
        if self.get(hash).is_some() {
            CcrEntryStatus {
                state: CcrEntryState::Available,
                ttl_seconds: self.default_ttl_seconds(),
                age_seconds: None,
                default_ttl_seconds: self.default_ttl_seconds(),
            }
        } else {
            CcrEntryStatus::missing(self.default_ttl_seconds())
        }
    }
}

/// Availability of one CCR entry. Port of the `"available" | "expired" |
/// "missing"` statuses from Python `get_entry_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcrEntryState {
    Available,
    Expired,
    Missing,
}

/// TTL metadata for one CCR entry. Port of the dict Python
/// `get_entry_status` returns (`status`, `ttl_seconds`, `age_seconds`,
/// `default_ttl_seconds`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CcrEntryStatus {
    pub state: CcrEntryState,
    pub ttl_seconds: u64,
    pub age_seconds: Option<u64>,
    pub default_ttl_seconds: u64,
}

impl CcrEntryStatus {
    pub fn missing(default_ttl_seconds: u64) -> Self {
        Self {
            state: CcrEntryState::Missing,
            ttl_seconds: 0,
            age_seconds: None,
            default_ttl_seconds,
        }
    }
}

/// Human-readable retrieval-miss detail. Port of Python
/// `format_retrieval_miss_detail`: an expired entry names its TTL and,
/// when known, its age; a missing entry names the store TTL.
pub fn format_retrieval_miss_detail(status: &CcrEntryStatus) -> String {
    match status.state {
        CcrEntryState::Available => "Entry available".to_string(),
        CcrEntryState::Expired => {
            if let Some(age) = status.age_seconds {
                format!(
                    "Entry expired (CCR TTL: {} seconds; age: {} seconds)",
                    status.ttl_seconds, age
                )
            } else {
                format!("Entry expired (CCR TTL: {} seconds)", status.ttl_seconds)
            }
        }
        CcrEntryState::Missing => format!(
            "Entry not found (CCR TTL: {} seconds)",
            status.default_ttl_seconds
        ),
    }
}

/// Default capacity — matches Python's `CompressionStore` default.
pub const DEFAULT_CAPACITY: usize = 1000;

/// Default TTL — 30 minutes, matching Python
/// (`CCRConfig.store_ttl_seconds`). Session-scale: agentic sessions
/// routinely outlive the old 5-minute default, and an expired entry
/// silently converts "lossless with retrieval" into "lossy".
pub const DEFAULT_TTL: Duration = Duration::from_secs(1800);

/// The TTL is an **idle window**, not a wall clock: every successful
/// `get` restarts the entry's clock, so an entry a session keeps
/// touching survives a long multi-agent burst (#2604). To keep
/// constant access from pinning an entry forever, an absolute max
/// lifetime of `DEFAULT_MAX_LIFETIME_MULTIPLIER * ttl` (measured from
/// insertion) caps the sliding window. Mirrors the Python
/// `CompressionStore` semantics.
pub const DEFAULT_MAX_LIFETIME_MULTIPLIER: u32 = 8;

/// Absolute max lifetime for an entry with idle window `idle_ttl`.
pub fn max_lifetime_for(idle_ttl: Duration) -> Duration {
    idle_ttl.saturating_mul(DEFAULT_MAX_LIFETIME_MULTIPLIER)
}

/// Compute the canonical CCR key for `payload`. BLAKE3 → first 24 hex
/// chars (96 bits — collision-resistant for the bounded LRU population
/// the proxy will hold). Centralized here so every call site (live-zone
/// dispatcher, tests, future Python parity) hashes the same way.
pub fn compute_key(payload: &[u8]) -> String {
    let h = blake3::hash(payload);
    let hex = h.to_hex();
    // Stable 24-char prefix matches the Python tool-injection regex
    // (`[a-f0-9]{24}`) — see `headroom/ccr/tool_injection.py:211`.
    hex.as_str()[..24].to_string()
}

/// Standard `<<ccr:HASH>>` marker injected into compressed block content
/// so the runtime can later look up the original bytes when the model
/// calls `headroom_retrieve`. Format is intentionally fixed across
/// proxy code-paths and tests.
pub fn marker_for(hash: &str) -> String {
    format!("<<ccr:{hash}>>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_key_is_24_hex_chars() {
        let k = compute_key(b"hello world");
        assert_eq!(k.len(), 24);
        assert!(
            k.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn compute_key_is_deterministic() {
        let a = compute_key(b"the same payload");
        let b = compute_key(b"the same payload");
        assert_eq!(a, b);
    }

    #[test]
    fn compute_key_diverges_for_different_payloads() {
        let a = compute_key(b"alpha");
        let b = compute_key(b"beta");
        assert_ne!(a, b);
    }

    #[test]
    fn marker_format_is_pinned() {
        assert_eq!(marker_for("abc123"), "<<ccr:abc123>>");
    }

    #[test]
    fn miss_detail_matches_upstream_shapes() {
        assert_eq!(
            format_retrieval_miss_detail(&CcrEntryStatus::missing(604_800)),
            "Entry not found (CCR TTL: 604800 seconds)"
        );
        assert_eq!(
            format_retrieval_miss_detail(&CcrEntryStatus {
                state: CcrEntryState::Expired,
                ttl_seconds: 60,
                age_seconds: Some(150),
                default_ttl_seconds: 60,
            }),
            "Entry expired (CCR TTL: 60 seconds; age: 150 seconds)"
        );
        assert_eq!(
            format_retrieval_miss_detail(&CcrEntryStatus {
                state: CcrEntryState::Expired,
                ttl_seconds: 60,
                age_seconds: None,
                default_ttl_seconds: 60,
            }),
            "Entry expired (CCR TTL: 60 seconds)"
        );
    }
}
