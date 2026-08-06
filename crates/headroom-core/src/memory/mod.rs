//! Hierarchical memory subsystem (Rust port of `headroom/memory/`).
//!
//! Ports the core data model, backend trait, and storage routing from Python.
//! The full orchestrator (`HierarchicalMemory`, `MemorySystem`) and backends
//! (`LocalBackend`, `DirectMem0Adapter`) stay Python-side for now — they
//! depend on sqlite-vec, ONNX embeddings, and the mem0 library.

pub mod backend;
pub mod models;
pub mod router;
