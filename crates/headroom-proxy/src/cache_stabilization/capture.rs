//! Phase J PR-J0 — env-gated request-body capture for the offload simulator.
//!
//! Records inbound request bodies to disk so the offline offload simulator
//! (`src/bin/offload_sim.rs`) can replay real sessions and sweep design
//! variants *before* any production offload code is written.
//!
//! # Invariant preservation
//!
//! Like the Phase E detectors, this is a **pure observer**: it never mutates
//! the request body and runs only when `HEADROOM_CAPTURE_DIR` is set (default
//! off). The proxy's "passthrough is sacred" invariant is preserved by
//! construction.
//!
//! # Privacy
//!
//! - Only the **body** is written (system / tools / messages) — never headers,
//!   where the API key / bearer token live. The capture path is physically
//!   incapable of writing a credential.
//! - `session_key` is the already-hashed drift session key (a SHA-256 prefix),
//!   never a raw secret.
//! - Capture is opt-in, local-only, and intended for the operator's own
//!   sessions on their own machine. Treat the corpus as private conversation
//!   data and delete it after a sweep.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// Monotonic per-process sequence so the simulator can order turns even when
/// wall-clock timestamps collide at millisecond resolution.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Per-process filename prefix. `SEQ` restarts at zero every process, so a
/// restart pointed at an existing corpus used to overwrite the previous run's
/// files one for one — the earlier capture was gone before anyone read it.
/// Ordering still comes from the `seq` field inside the envelope, so this only
/// has to make names unique.
fn run_id() -> u64 {
    static RUN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *RUN.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    })
}

/// Append `parsed` to the capture corpus when `HEADROOM_CAPTURE_DIR` is set.
///
/// No-op (one cheap `env::var`) when the env var is absent — the common path.
///
/// # Non-blocking
///
/// All serialization + disk I/O happens on a **detached background thread**, so
/// the request path never blocks on `fs::write` (which on Windows is
/// synchronously scanned by Defender and can stall the async worker — the cause
/// of the J0 capture hang). The request thread only clones the body once and
/// spawns; write failures are logged at WARN off-thread and swallowed: capture
/// must never break — or slow — a live request.
pub fn maybe_capture(parsed: &Value, endpoint: &str, session_key: &str, request_id: &str) {
    let dir = match std::env::var("HEADROOM_CAPTURE_DIR") {
        Ok(d) if !d.is_empty() => d,
        _ => return,
    };

    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    // Take owned copies and hand everything to a detached writer thread. The
    // body clone is the only cost on the request path (a memcpy); the
    // serialize + Defender-scanned write run off the hot path.
    let body = parsed.clone();
    let endpoint = endpoint.to_string();
    let session_key = session_key.to_string();
    let request_id = request_id.to_string();

    std::thread::spawn(move || {
        let envelope = serde_json::json!({
            "seq": seq,
            "ts_ms": ts_ms,
            "endpoint": endpoint,
            "session_key": session_key,
            "request_id": request_id,
            "body": body,
        });

        let path = std::path::Path::new(&dir).join(format!("req-{}-{seq:08}.json", run_id()));
        let write = std::fs::create_dir_all(&dir)
            .and_then(|_| serde_json::to_vec(&envelope).map_err(std::io::Error::other))
            .and_then(|bytes| std::fs::write(&path, bytes));

        if let Err(e) = write {
            tracing::warn!(
                event = "capture_write_failed",
                request_id = %request_id,
                error = %e,
                "J0 capture: failed to write request body (continuing)"
            );
        }
    });
}

/// Append the final wire bytes for `request_id` to `$HEADROOM_CAPTURE_DIR/out`.
///
/// Same env gate and same never-block rule as [`maybe_capture`]. Pairs with the
/// inbound capture by `request_id`, so a request can be diffed as the client
/// sent it against what actually went upstream.
pub fn maybe_capture_outbound(body: &[u8], request_id: &str) {
    let dir = match std::env::var("HEADROOM_CAPTURE_DIR") {
        Ok(d) if !d.is_empty() => d,
        _ => return,
    };

    let body = body.to_vec();
    let request_id = request_id.to_string();

    std::thread::spawn(move || {
        let dir = std::path::Path::new(&dir).join("out");
        let path = dir.join(format!("{request_id}.json"));
        let write = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &body));

        if let Err(e) = write {
            tracing::warn!(
                event = "capture_outbound_write_failed",
                request_id = %request_id,
                error = %e,
                "outbound capture: failed to write wire body (continuing)"
            );
        }
    });
}
