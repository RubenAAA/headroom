//! Canonical filesystem contract for Headroom (Rust port of `headroom/paths.py`).
//!
//! Two canonical roots:
//!
//! * `HEADROOM_CONFIG_DIR` — read-mostly configuration (defaults to
//!   `~/.headroom/config`).
//! * `HEADROOM_WORKSPACE_DIR` — read-write state (defaults to `~/.headroom`).
//!
//! Precedence for every per-resource helper is:
//!
//! ```text
//! explicit argument > per-resource env var > derived from canonical root > default
//! ```
//!
//! Helpers are pure — they never `mkdir` (use the `ensure_*` variants). No
//! caching: every call re-reads the environment so test overrides take effect
//! without extra hoops.
//!
//! This is a partial port: only the roots + the resources the subscription
//! stack and savings ledger need are carried over. Python-only resources
//! (dashboard, memory DB, vendored-binary paths, plugin dirs, …) are omitted;
//! the generic `resolve` helper is kept so adding them later is mechanical.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Canonical env var names
// ---------------------------------------------------------------------------

pub const HEADROOM_CONFIG_DIR_ENV: &str = "HEADROOM_CONFIG_DIR";
pub const HEADROOM_WORKSPACE_DIR_ENV: &str = "HEADROOM_WORKSPACE_DIR";

// Legacy per-resource env vars (kept for backward compatibility).
pub const HEADROOM_SAVINGS_PATH_ENV: &str = "HEADROOM_SAVINGS_PATH";
pub const HEADROOM_SAVINGS_EVENTS_PATH_ENV: &str = "HEADROOM_SAVINGS_EVENTS_PATH";
pub const HEADROOM_TOIN_PATH_ENV: &str = "HEADROOM_TOIN_PATH";
pub const HEADROOM_SUBSCRIPTION_STATE_PATH_ENV: &str = "HEADROOM_SUBSCRIPTION_STATE_PATH";
pub const HEADROOM_COPILOT_AUTH_FILE_ENV: &str = "HEADROOM_COPILOT_AUTH_FILE";

// Default sub-path fragments.
const WORKSPACE_DIR_DEFAULT: &str = ".headroom";
const CONFIG_DIR_DEFAULT_SUFFIX: &str = "config";

const SAVINGS_FILE: &str = "proxy_savings.json";
const OUTPUT_SAVINGS_FILE: &str = "output_savings.json";
const TOIN_FILE: &str = "toin.json";
const SUBSCRIPTION_FILE: &str = "subscription_state.json";
const SAVINGS_EVENTS_FILE: &str = "savings_events.jsonl";
const COPILOT_AUTH_FILE: &str = "copilot_auth.json";

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Return a trimmed environment value, or `None` when unset/blank.
fn env(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Expand a leading `~` to the user's home directory, mirroring Python's
/// `Path.expanduser()`.
fn expanduser(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~") {
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
            if let Some(home) = home_dir() {
                let rest = rest.trim_start_matches(['/', '\\']);
                if rest.is_empty() {
                    return home;
                }
                return home.join(rest);
            }
        }
    }
    PathBuf::from(value)
}

fn home_dir() -> Option<PathBuf> {
    // Match Python's `Path.home()`: honour $HOME first, then platform default.
    if let Some(home) = env("HOME") {
        return Some(PathBuf::from(home));
    }
    #[cfg(windows)]
    if let Some(profile) = env("USERPROFILE") {
        return Some(PathBuf::from(profile));
    }
    None
}

/// Apply the standard precedence: explicit > env > derived. `explicit` and the
/// env value are both `expanduser`-expanded.
fn resolve(explicit: Option<&Path>, env_var: &str, derived: PathBuf) -> PathBuf {
    if let Some(path) = explicit {
        if !path.as_os_str().is_empty() {
            return expanduser(&path.to_string_lossy());
        }
    }
    if let Some(env_value) = env(env_var) {
        return expanduser(&env_value);
    }
    derived
}

// ---------------------------------------------------------------------------
// Canonical roots
// ---------------------------------------------------------------------------

/// Workspace (read-write state) root. `$HEADROOM_WORKSPACE_DIR` or `~/.headroom`.
pub fn workspace_dir() -> PathBuf {
    if let Some(value) = env(HEADROOM_WORKSPACE_DIR_ENV) {
        return expanduser(&value);
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(WORKSPACE_DIR_DEFAULT)
}

/// Config (read-mostly) root.
///
/// 1. `$HEADROOM_CONFIG_DIR` if set.
/// 2. `$HEADROOM_WORKSPACE_DIR/config` when the workspace env var is set.
/// 3. `~/.headroom/config` otherwise.
pub fn config_dir() -> PathBuf {
    if let Some(value) = env(HEADROOM_CONFIG_DIR_ENV) {
        return expanduser(&value);
    }
    if let Some(workspace) = env(HEADROOM_WORKSPACE_DIR_ENV) {
        return expanduser(&workspace).join(CONFIG_DIR_DEFAULT_SUFFIX);
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(WORKSPACE_DIR_DEFAULT)
        .join(CONFIG_DIR_DEFAULT_SUFFIX)
}

/// Return [`workspace_dir`], creating it if it does not yet exist.
pub fn ensure_workspace_dir() -> std::io::Result<PathBuf> {
    let path = workspace_dir();
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

/// Return [`config_dir`], creating it if it does not yet exist.
pub fn ensure_config_dir() -> std::io::Result<PathBuf> {
    let path = config_dir();
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Per-resource helpers — workspace bucket
// ---------------------------------------------------------------------------

/// Path for the proxy savings JSON ledger.
pub fn savings_path(explicit: Option<&Path>) -> PathBuf {
    resolve(
        explicit,
        HEADROOM_SAVINGS_PATH_ENV,
        workspace_dir().join(SAVINGS_FILE),
    )
}

/// Path for the output-token savings ledger.
pub fn output_savings_path() -> PathBuf {
    workspace_dir().join(OUTPUT_SAVINGS_FILE)
}

/// Path for the GitHub Copilot OAuth token cache.
pub fn copilot_auth_path() -> PathBuf {
    resolve(
        None,
        HEADROOM_COPILOT_AUTH_FILE_ENV,
        workspace_dir().join(COPILOT_AUTH_FILE),
    )
}

/// Path for the TOIN telemetry JSON file.
pub fn toin_path(explicit: Option<&Path>) -> PathBuf {
    resolve(
        explicit,
        HEADROOM_TOIN_PATH_ENV,
        workspace_dir().join(TOIN_FILE),
    )
}

/// Path for the subscription tracker state JSON.
pub fn subscription_state_path(explicit: Option<&Path>) -> PathBuf {
    resolve(
        explicit,
        HEADROOM_SUBSCRIPTION_STATE_PATH_ENV,
        workspace_dir().join(SUBSCRIPTION_FILE),
    )
}

/// Path for the durable append-only savings event ledger.
pub fn savings_events_path(explicit: Option<&Path>) -> PathBuf {
    resolve(
        explicit,
        HEADROOM_SAVINGS_EVENTS_PATH_ENV,
        workspace_dir().join(SAVINGS_EVENTS_FILE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    // The tests mutate process-wide env vars; serialize them.
    fn env_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn workspace_dir_uses_env_override() {
        let _g = env_guard();
        std::env::set_var(HEADROOM_WORKSPACE_DIR_ENV, "/tmp/hr-ws");
        assert_eq!(workspace_dir(), PathBuf::from("/tmp/hr-ws"));
        std::env::remove_var(HEADROOM_WORKSPACE_DIR_ENV);
    }

    #[test]
    fn config_dir_derives_from_workspace_env() {
        let _g = env_guard();
        std::env::remove_var(HEADROOM_CONFIG_DIR_ENV);
        std::env::set_var(HEADROOM_WORKSPACE_DIR_ENV, "/tmp/hr-ws");
        assert_eq!(config_dir(), PathBuf::from("/tmp/hr-ws/config"));
        std::env::remove_var(HEADROOM_WORKSPACE_DIR_ENV);
    }

    #[test]
    fn subscription_path_precedence_explicit_over_env() {
        let _g = env_guard();
        std::env::set_var(HEADROOM_SUBSCRIPTION_STATE_PATH_ENV, "/tmp/from-env.json");
        let explicit = PathBuf::from("/tmp/explicit.json");
        assert_eq!(
            subscription_state_path(Some(&explicit)),
            PathBuf::from("/tmp/explicit.json")
        );
        assert_eq!(
            subscription_state_path(None),
            PathBuf::from("/tmp/from-env.json")
        );
        std::env::remove_var(HEADROOM_SUBSCRIPTION_STATE_PATH_ENV);
    }

    #[test]
    fn savings_events_path_env_override() {
        let _g = env_guard();
        std::env::set_var(HEADROOM_SAVINGS_EVENTS_PATH_ENV, "/tmp/ev.jsonl");
        assert_eq!(savings_events_path(None), PathBuf::from("/tmp/ev.jsonl"));
        std::env::remove_var(HEADROOM_SAVINGS_EVENTS_PATH_ENV);
    }

    #[test]
    fn output_savings_path_derives_from_workspace_without_env_override() {
        let _g = env_guard();
        std::env::set_var(HEADROOM_WORKSPACE_DIR_ENV, "/tmp/hr-ws");
        assert_eq!(
            output_savings_path(),
            PathBuf::from("/tmp/hr-ws/output_savings.json")
        );
        std::env::remove_var(HEADROOM_WORKSPACE_DIR_ENV);
    }

    #[test]
    fn copilot_auth_path_honors_resource_env_override() {
        let _g = env_guard();
        std::env::set_var(HEADROOM_WORKSPACE_DIR_ENV, "/tmp/hr-ws");
        std::env::set_var(HEADROOM_COPILOT_AUTH_FILE_ENV, "/tmp/copilot.json");
        assert_eq!(copilot_auth_path(), PathBuf::from("/tmp/copilot.json"));
        std::env::remove_var(HEADROOM_COPILOT_AUTH_FILE_ENV);
        std::env::remove_var(HEADROOM_WORKSPACE_DIR_ENV);
    }
}
