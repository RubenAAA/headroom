//! Lightweight audit log for administrative / state-mutating proxy actions.
//!
//! Emits structured JSON events for sensitive actions. In Rust the actual
//! logging goes through `tracing`/`log`; this module provides the path
//! classification logic.

/// Paths whose requests mutate runtime state or expose stored content.
const ADMIN_PREFIX: &str = "/admin/";
const SENSITIVE_EXACT: &[&str] = &["/cache/clear", "/stats/reset"];

/// Return true when requests to `path` should be audited.
pub fn is_auditable_path(path: &str) -> bool {
    path.starts_with(ADMIN_PREFIX) || SENSITIVE_EXACT.contains(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_paths_are_auditable() {
        assert!(is_auditable_path("/admin/runtime-env"));
        assert!(is_auditable_path("/admin/cache"));
    }

    #[test]
    fn sensitive_exact_paths_are_auditable() {
        assert!(is_auditable_path("/cache/clear"));
        assert!(is_auditable_path("/stats/reset"));
    }

    #[test]
    fn normal_paths_are_not_auditable() {
        assert!(!is_auditable_path("/v1/messages"));
        assert!(!is_auditable_path("/health"));
        assert!(!is_auditable_path("/stats"));
    }

    #[test]
    fn admin_prefix_requires_slash() {
        assert!(!is_auditable_path("admin/foo"));
    }
}
