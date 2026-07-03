//! Context-mode passive capture (CTX-2a) for the proxy.
//!
//! - [`identity`] — conversation fingerprint (`conversation_key`) and the
//!   new/continuation/compaction-or-resume/branch classifier over the rolling
//!   prefix-chain persisted in `headroom_core::ctx::SessionsStore`.
//! - [`extract`] — event extraction skeleton mirroring `session/extract.ts`;
//!   four categories (intent, error, file, git) implemented, the rest stubbed
//!   as `// CTX-2b:`.
//! - [`observer`] — the detached background worker that ties the two together
//!   and writes to the sessions DB, following `capture.rs`'s never-block rule.
//! - [`offload_store`] (CTX-3) — the background sink that persists offloaded
//!   `tool_result` originals into the CCR store + FTS content index. The
//!   offload *transform* itself lives in `compression::ctx_offload`; this
//!   module only handles the storage side effect (never touches wire bytes).
//!
//! Everything here is a pure observer: no request/response byte is mutated and
//! no latency is added to the request path.

pub mod extract;
pub mod identity;
pub mod observer;
pub mod offload_store;
