//! OAuth token resolution for the Anthropic usage API (port of
//! `headroom/subscription/client.py`).
//!
//! Token resolution order (highest → lowest priority):
//!   1. Explicit token passed to the fetcher.
//!   2. `CLAUDE_CODE_OAUTH_TOKEN` env var.
//!   3. `~/.claude/.credentials.json` → `claudeAiOauth.accessToken`
//!      (respects `CLAUDE_CONFIG_DIR`).
//!
//! Token resolution needs no HTTP, so it lives in core. The actual
//! `GET https://api.anthropic.com/api/oauth/usage` call is behind the
//! [`SubscriptionFetcher`] trait — the reqwest-backed impl and the async poll
//! loop live in `headroom-proxy` so core gains no heavy HTTP/async dependency.

use std::path::PathBuf;

use chrono::Utc;
use serde_json::Value;

use super::models::SubscriptionSnapshot;

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const BETA_HEADER: &str = "oauth-2025-04-20";
/// Seconds of expiry buffer before a cached token is considered unusable.
pub const TOKEN_EXPIRY_BUFFER_S: i64 = 60;

fn credentials_path() -> PathBuf {
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".claude")
        });
    base.join(".credentials.json")
}

fn load_credentials_file() -> Option<Value> {
    let path = credentials_path();
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Resolve a stored OAuth token for background polling (no request needed).
///
/// Returns the raw access token if found and not expired, else `None`.
pub fn read_cached_oauth_token() -> Option<String> {
    // 1. Env var.
    if let Ok(env_token) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        let trimmed = env_token.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    // 2. Credentials file.
    let creds = load_credentials_file()?;
    let oauth = creds.get("claudeAiOauth")?;
    let token = oauth.get("accessToken").and_then(|v| v.as_str())?;
    if token.is_empty() {
        return None;
    }

    // Check expiry (Anthropic stores the timestamp in milliseconds).
    if let Some(expires_at_ms) = oauth.get("expiresAt").and_then(|v| v.as_f64()) {
        let now_ms = Utc::now().timestamp_millis() as f64;
        if now_ms >= (expires_at_ms - TOKEN_EXPIRY_BUFFER_S as f64 * 1000.0) {
            return None;
        }
    }

    Some(token.to_string())
}

/// Abstraction over "fetch one subscription snapshot from the usage API".
///
/// Core defines the trait; `headroom-proxy` provides the reqwest-backed impl.
/// Injecting a fake makes the tracker's reconciliation logic testable without
/// network access.
pub trait SubscriptionFetcher: Send + Sync {
    /// Fetch the current subscription window data. `None` on auth failure /
    /// unsupported account. When `token` is `None`, implementations should fall
    /// back to [`read_cached_oauth_token`].
    fn fetch(&self, token: Option<&str>) -> Option<SubscriptionSnapshot>;
}

#[cfg(test)]
mod tests {
    use super::*;
    // Shared with `session_tracking`'s tests: both read `CLAUDE_CONFIG_DIR`.
    use crate::subscription::env_guard;

    #[test]
    fn env_token_takes_priority() {
        let _g = env_guard();
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "  env-token  ");
        assert_eq!(read_cached_oauth_token().as_deref(), Some("env-token"));
        std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
    }

    #[test]
    fn reads_unexpired_token_from_credentials_file() {
        let _g = env_guard();
        std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        let dir = tempfile::tempdir().unwrap();
        let future_ms = (Utc::now().timestamp_millis() + 3_600_000) as f64;
        std::fs::write(
            dir.path().join(".credentials.json"),
            serde_json::json!({
                "claudeAiOauth": {"accessToken": "file-token", "expiresAt": future_ms}
            })
            .to_string(),
        )
        .unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", dir.path());
        assert_eq!(read_cached_oauth_token().as_deref(), Some("file-token"));
        std::env::remove_var("CLAUDE_CONFIG_DIR");
    }

    #[test]
    fn expired_token_returns_none() {
        let _g = env_guard();
        std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        let dir = tempfile::tempdir().unwrap();
        let past_ms = (Utc::now().timestamp_millis() - 1000) as f64;
        std::fs::write(
            dir.path().join(".credentials.json"),
            serde_json::json!({
                "claudeAiOauth": {"accessToken": "old", "expiresAt": past_ms}
            })
            .to_string(),
        )
        .unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", dir.path());
        assert_eq!(read_cached_oauth_token(), None);
        std::env::remove_var("CLAUDE_CONFIG_DIR");
    }
}
