//! Context-mode storage foundation (phase CTX-1).
//!
//! A Rust port of the TypeScript context-mode content store
//! (`context-mode/src/store.ts` + `src/search/unified.ts`): an FTS5 BM25
//! knowledge base with a dual porter/trigram index merged via Reciprocal Rank
//! Fusion, proximity re-ranking, Levenshtein typo correction, and a
//! markdown/blank-line chunker.
//!
//! The schema is **byte-compatible** with the TS store, so a context-mode DB
//! at `~/.claude-personal/context-mode/content/<hash>.db` opens cleanly if one
//! is pointed at with `--ctx-store-dir`. Headroom's own store defaults to
//! `<workspace>/ctx` (see [`store::default_base_dir`]) — schema compatibility
//! is not a reason to write into another tool's state directory.
//!
//! Later phases (CTX-2+) add passive session capture, the tool_result offload
//! transform, recall injection, and the CLI/HTTP surface. This module owns
//! only the storage + search primitives.

mod memory_records;
mod sessions;
mod snapshot;
mod store;

pub use memory_records::MemoryRecordStore;
pub use sessions::{data_hash, session_db_path, NewEvent, PrefixTurn, SessionsStore, StoredEvent};
pub use snapshot::{build_recall, build_resume_snapshot, INJECT_SENTINEL};
pub use store::{
    content_db_path, default_base_dir, hash_project_dir_canonical, sanitize_query,
    sanitize_trigram_query, ContentType, CtxStore, IndexOpts, IndexSummary, SearchHit, SearchOpts,
    SortMode, SourceMeta, RRF_K,
};
