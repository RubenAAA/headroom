//! Fail-closed decision matrix for when the compression pipeline errors on a
//! Realtime WebSocket frame (or analogous HTTP body).
//!
//! Rust port of `headroom/proxy/helpers.py` (~L800-916:
//! `decide_compression_failure_action` / `CompressionFailureAction`).
//!
//! Default behaviour is fail-CLOSED: refuse to forward the original bytes so
//! the client learns to compact and retry, rather than silently forwarding an
//! oversized frame that overflows the model context. The decision is exposed
//! here as a PURE function over explicit params; env/exception inspection
//! lives at the call site (and in [`oversize_threshold_bytes`]).

/// Env var operators set to opt back into legacy fail-open behaviour.
pub const WS_COMPRESSION_FAIL_OPEN_ENV: &str = "HEADROOM_WS_FAIL_OPEN_ON_COMPRESSION_FAILURE";
/// Env var overriding the oversize threshold (bytes).
pub const WS_COMPRESSION_OVERSIZE_BYTES_ENV: &str = "HEADROOM_WS_COMPRESSION_FAIL_THRESHOLD_BYTES";
/// Default oversize threshold: 256 KiB (~64K tokens).
pub const WS_COMPRESSION_OVERSIZE_BYTES_DEFAULT: usize = 256 * 1024;

/// Decision returned by [`decide_compression_failure_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionFailureAction {
    /// If true, the caller MUST NOT forward the original frame — close the
    /// client connection with a clear error code instead.
    pub refuse: bool,
    /// Short machine-readable label for telemetry.
    pub reason: String,
    /// Original frame size in bytes (for logging / metrics).
    pub frame_bytes: usize,
}

/// Resolve the oversize threshold from an optional operator-supplied env value.
///
/// Mirrors the Python parsing: a positive integer overrides the default; a
/// blank or non-integer value keeps [`WS_COMPRESSION_OVERSIZE_BYTES_DEFAULT`].
/// Pass the raw `HEADROOM_WS_COMPRESSION_FAIL_THRESHOLD_BYTES` value (trimmed
/// or not); `None` means unset.
pub fn oversize_threshold_bytes(raw_env: Option<&str>) -> usize {
    if let Some(raw) = raw_env {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            if let Ok(parsed) = trimmed.parse::<usize>() {
                if parsed > 0 {
                    return parsed;
                }
            }
        }
    }
    WS_COMPRESSION_OVERSIZE_BYTES_DEFAULT
}

/// Decide whether to refuse-and-close vs forward-original after the
/// compression pipeline fails.
///
/// Pure function; the caller resolves the params:
///   * `fail_open_env_override` — `HEADROOM_WS_FAIL_OPEN_ON_COMPRESSION_FAILURE`
///     is truthy.
///   * `is_codex_client` — request client is `codex`.
///   * `is_timeout` — the failure was the compression stage's own timeout.
///   * `frame_bytes` — original frame size.
///   * `threshold_bytes` — from [`oversize_threshold_bytes`].
///
/// Precedence (matches Python):
///   env override → forward; codex+timeout → forward; timeout → refuse;
///   frame_bytes > threshold → refuse; else forward.
pub fn decide_compression_failure_action(
    fail_open_env_override: bool,
    is_codex_client: bool,
    is_timeout: bool,
    frame_bytes: usize,
    threshold_bytes: usize,
) -> CompressionFailureAction {
    if fail_open_env_override {
        return CompressionFailureAction {
            refuse: false,
            reason: "env_override:fail_open".to_string(),
            frame_bytes,
        };
    }

    if is_codex_client && is_timeout {
        return CompressionFailureAction {
            refuse: false,
            reason: "client_override:codex".to_string(),
            frame_bytes,
        };
    }

    if is_timeout {
        return CompressionFailureAction {
            refuse: true,
            reason: "timeout".to_string(),
            frame_bytes,
        };
    }

    if frame_bytes > threshold_bytes {
        return CompressionFailureAction {
            refuse: true,
            reason: format!("oversize:bytes={frame_bytes}>threshold={threshold_bytes}"),
            frame_bytes,
        };
    }

    CompressionFailureAction {
        refuse: false,
        reason: "small_frame_transient".to_string(),
        frame_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: usize = WS_COMPRESSION_OVERSIZE_BYTES_DEFAULT;

    #[test]
    fn test_env_override_forwards() {
        let a = decide_compression_failure_action(true, false, true, 10 * T, T);
        assert!(!a.refuse);
        assert_eq!(a.reason, "env_override:fail_open");
    }

    #[test]
    fn test_codex_timeout_forwards() {
        let a = decide_compression_failure_action(false, true, true, 10 * T, T);
        assert!(!a.refuse);
        assert_eq!(a.reason, "client_override:codex");
    }

    #[test]
    fn test_timeout_refuses() {
        let a = decide_compression_failure_action(false, false, true, 100, T);
        assert!(a.refuse);
        assert_eq!(a.reason, "timeout");
    }

    #[test]
    fn test_oversize_refuses() {
        let a = decide_compression_failure_action(false, false, false, T + 1, T);
        assert!(a.refuse);
        assert_eq!(
            a.reason,
            format!("oversize:bytes={}>threshold={}", T + 1, T)
        );
    }

    #[test]
    fn test_small_frame_transient_forwards() {
        let a = decide_compression_failure_action(false, false, false, 100, T);
        assert!(!a.refuse);
        assert_eq!(a.reason, "small_frame_transient");
    }

    #[test]
    fn test_codex_non_timeout_falls_through_to_oversize() {
        // Codex override only fires on timeout; a non-timeout oversize refuses.
        let a = decide_compression_failure_action(false, true, false, T + 1, T);
        assert!(a.refuse);
        assert!(a.reason.starts_with("oversize:"));
    }

    // ── oversize_threshold_bytes ─────────────────────────────────

    #[test]
    fn test_threshold_default_when_unset() {
        assert_eq!(oversize_threshold_bytes(None), T);
        assert_eq!(oversize_threshold_bytes(Some("  ")), T);
    }

    #[test]
    fn test_threshold_override_positive_int() {
        assert_eq!(oversize_threshold_bytes(Some("1024")), 1024);
        assert_eq!(oversize_threshold_bytes(Some(" 2048 ")), 2048);
    }

    #[test]
    fn test_threshold_ignores_bad_value() {
        assert_eq!(oversize_threshold_bytes(Some("nope")), T);
        assert_eq!(oversize_threshold_bytes(Some("0")), T);
    }
}
