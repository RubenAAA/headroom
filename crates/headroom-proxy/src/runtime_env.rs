//! Live (per-request) env knobs and a hot-reload override store.
//!
//! Most Headroom settings are read once at proxy startup. A second, smaller
//! class of environment variables is read *live* on every request. This module
//! provides a process-global override store so `headroom wrap` can push values
//! without restarting the proxy.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

/// One reuse-invalidating live env var.
#[derive(Debug, Clone)]
pub struct Knob {
    pub env: String,
    pub kind: String,
    pub summary: String,
}

/// The registry of live knobs.
pub fn runtime_env_knobs() -> Vec<Knob> {
    vec![
        Knob {
            env: "HEADROOM_OUTPUT_SHAPER".into(),
            kind: "bool".into(),
            summary: "Master switch for output-token shaping.".into(),
        },
        Knob {
            env: "HEADROOM_VERBOSITY_LEVEL".into(),
            kind: "int".into(),
            summary: "Verbosity steering level 0-4.".into(),
        },
        Knob {
            env: "HEADROOM_EFFORT_ROUTER".into(),
            kind: "bool".into(),
            summary: "Lower effort on mechanical tool-result continuations.".into(),
        },
        Knob {
            env: "HEADROOM_MECHANICAL_EFFORT".into(),
            kind: "str".into(),
            summary: "Effort value used on mechanical continuations.".into(),
        },
        Knob {
            env: "HEADROOM_VERBOSITY_AUTOTUNE".into(),
            kind: "bool".into(),
            summary: "Use the AIMD verbosity controller state.".into(),
        },
        Knob {
            env: "HEADROOM_OUTPUT_HOLDOUT".into(),
            kind: "float".into(),
            summary: "Fraction of conversations held out for A/B measurement.".into(),
        },
        Knob {
            env: "HEADROOM_INTERCEPT_READ_MIN_CHARS".into(),
            kind: "int".into(),
            summary: "Min tool-output chars before the ast-grep read rewrite.".into(),
        },
    ]
}

static OVERRIDES: LazyLock<RwLock<HashMap<String, String>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Serializes tests that mutate the process-global [`OVERRIDES`] store.
///
/// `set_overrides` / `clear_overrides` are global, and the test runner is
/// multi-threaded: without this, one test's `clear_overrides()` lands between
/// another's `set_overrides()` and its assertion, and the reader sees the
/// default instead of the override. That produced a genuinely intermittent
/// failure — the suite passed five runs in a row and then failed once.
///
/// Every test that mutates overrides must hold this for its whole body. Poison
/// is recovered rather than propagated so one failing test doesn't cascade into
/// unrelated ones.
#[cfg(test)]
pub(crate) fn override_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Return the live value for `name`: hot-reload override, else environment.
pub fn getenv(name: &str, default: Option<&str>) -> Option<String> {
    // Check overrides first (read lock, non-blocking for readers)
    {
        let overrides = OVERRIDES.read().unwrap();
        if let Some(val) = overrides.get(name) {
            return Some(val.clone());
        }
    }
    // Fall back to environment
    std::env::var(name)
        .ok()
        .or_else(|| default.map(|s| s.to_string()))
}

/// Apply hot-reload overrides for known knobs. Returns what was applied.
pub fn set_overrides(values: &HashMap<String, String>) -> HashMap<String, String> {
    let known: HashMap<String, ()> = runtime_env_knobs()
        .iter()
        .map(|k| (k.env.clone(), ()))
        .collect();

    let mut applied = HashMap::new();
    let mut overrides = OVERRIDES.write().unwrap();
    for (key, value) in values {
        if known.contains_key(key) {
            overrides.insert(key.clone(), value.clone());
            applied.insert(key.clone(), value.clone());
        }
    }
    applied
}

/// Drop all overrides (used by tests and to reset state).
pub fn clear_overrides() {
    let mut overrides = OVERRIDES.write().unwrap();
    overrides.clear();
}

/// Knobs *explicitly* set (non-empty) in `environ`.
pub fn explicit_env(environ: &HashMap<String, String>) -> HashMap<String, String> {
    let knobs = runtime_env_knobs();
    let mut out = HashMap::new();
    for knob in &knobs {
        if let Some(raw) = environ.get(&knob.env) {
            if !raw.trim().is_empty() {
                out.insert(knob.env.clone(), raw.clone());
            }
        }
    }
    out
}

/// The live value of every knob (override-or-environment) for `/health`.
pub fn effective_runtime_env() -> HashMap<String, Option<String>> {
    runtime_env_knobs()
        .iter()
        .map(|knob| (knob.env.clone(), getenv(&knob.env, None)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getenv_falls_back_to_default() {
        // Serialize: the override store is process-global (see
        // `override_test_lock`), and the runner is multi-threaded.
        let _guard = override_test_lock();
        clear_overrides();
        let val = getenv("DEFINITELY_NOT_SET_VAR_XYZ", Some("fallback"));
        assert_eq!(val.as_deref(), Some("fallback"));
    }

    #[test]
    fn getenv_returns_none_when_unset() {
        // Serialize: the override store is process-global (see
        // `override_test_lock`), and the runner is multi-threaded.
        let _guard = override_test_lock();
        clear_overrides();
        let val = getenv("DEFINITELY_NOT_SET_VAR_XYZ", None);
        assert!(val.is_none());
    }

    #[test]
    fn override_wins_over_env() {
        // Serialize: the override store is process-global (see
        // `override_test_lock`), and the runner is multi-threaded.
        let _guard = override_test_lock();
        clear_overrides();
        let mut overrides = HashMap::new();
        overrides.insert("HEADROOM_OUTPUT_SHAPER".to_string(), "1".to_string());
        set_overrides(&overrides);
        let val = getenv("HEADROOM_OUTPUT_SHAPER", None);
        assert_eq!(val.as_deref(), Some("1"));
        clear_overrides();
    }

    #[test]
    fn clear_overrides_removes_all() {
        // Serialize: the override store is process-global (see
        // `override_test_lock`), and the runner is multi-threaded.
        let _guard = override_test_lock();
        let mut overrides = HashMap::new();
        overrides.insert("HEADROOM_VERBOSITY_LEVEL".to_string(), "3".to_string());
        set_overrides(&overrides);
        clear_overrides();
        // After clear, should fall back to env (which is unset)
        let val = getenv("HEADROOM_VERBOSITY_LEVEL", None);
        assert!(val.is_none());
    }

    #[test]
    fn explicit_env_captures_non_empty() {
        let mut environ = HashMap::new();
        environ.insert("HEADROOM_OUTPUT_SHAPER".to_string(), "true".to_string());
        environ.insert("HEADROOM_VERBOSITY_LEVEL".to_string(), "".to_string());
        let explicit = explicit_env(&environ);
        assert!(explicit.contains_key("HEADROOM_OUTPUT_SHAPER"));
        assert!(!explicit.contains_key("HEADROOM_VERBOSITY_LEVEL"));
    }

    #[test]
    fn effective_runtime_env_covers_all_knobs() {
        // Serialize: the override store is process-global (see
        // `override_test_lock`), and the runner is multi-threaded.
        let _guard = override_test_lock();
        clear_overrides();
        let env = effective_runtime_env();
        assert!(env.contains_key("HEADROOM_OUTPUT_SHAPER"));
        assert!(env.contains_key("HEADROOM_VERBOSITY_LEVEL"));
    }

    #[test]
    fn knobs_registry_is_non_empty() {
        let knobs = runtime_env_knobs();
        assert!(!knobs.is_empty());
    }
}
