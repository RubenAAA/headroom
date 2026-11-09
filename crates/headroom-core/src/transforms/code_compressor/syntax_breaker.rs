//! Per-language syntax breaker, moved out of `code_compressor.rs`.
//!
//! The AST pass validates its own output and discards anything that no longer
//! parses ("never serve broken code"), so a misbehaving grammar costs the full
//! compression latency and then throws the work away. When a language keeps
//! failing its own validation, stop attempting it for a cooldown instead of
//! burning the latency on every request.
//!
//! Process-global, deliberately: a failing grammar is a property of the
//! process's tree-sitter build, not of the caller. Opt out per-deployment
//! with `HEADROOM_CODE_SYNTAX_BREAKER=0` (also `false`/`off`), read live on
//! every call like the other kill-switches in this crate.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(super) const SYNTAX_BREAKER_WINDOW: usize = 20;
pub(super) const SYNTAX_BREAKER_MIN_FAILURES: usize = 10;
pub(super) const SYNTAX_BREAKER_COOLDOWN_SECS: u64 = 1800;
pub(super) const SYNTAX_BREAKER_ENV: &str = "HEADROOM_CODE_SYNTAX_BREAKER";

#[derive(Debug, Default)]
pub(super) struct SyntaxBreakerState {
    pub(super) outcomes: HashMap<String, VecDeque<bool>>,
    pub(super) open_until: HashMap<String, Instant>,
    pub(super) trips: HashMap<String, u64>,
    /// Attempts skipped because the input never parsed: input noise, not
    /// compressor failure. Reported on the trip line so a trip with a high
    /// skip count reads as "mostly fragments" rather than "broken grammar".
    pub(super) invalid_input_skips: HashMap<String, u64>,
}

pub(super) fn syntax_breaker_state() -> &'static Mutex<SyntaxBreakerState> {
    static STATE: OnceLock<Mutex<SyntaxBreakerState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(SyntaxBreakerState::default()))
}

/// False when the breaker is opted out via
/// `HEADROOM_CODE_SYNTAX_BREAKER=0|false|off` (case-insensitive, trimmed).
pub(super) fn syntax_breaker_enabled() -> bool {
    match std::env::var(SYNTAX_BREAKER_ENV) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        ),
        Err(_) => true,
    }
}

/// True while AST compression for `language` (a [`CodeLanguage::value`]) is
/// paused after repeat validation failures.
pub(super) fn syntax_breaker_open(language: &str) -> bool {
    if !syntax_breaker_enabled() {
        return false;
    }
    let state = syntax_breaker_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    state
        .open_until
        .get(language)
        .is_some_and(|until| *until > Instant::now())
}

/// Track a validation outcome; trip the breaker on a failing window. The
/// window clears on trip so a fresh run of failures must re-earn the next
/// pause. Other languages are isolated.
pub(super) fn record_syntax_outcome(language: &str, valid: bool) {
    if !syntax_breaker_enabled() {
        return;
    }
    let mut tripped_failures = 0usize;
    let mut tripped_skips = 0u64;
    {
        let mut state = syntax_breaker_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let failures = {
            let window = state.outcomes.entry(language.to_string()).or_default();
            if window.len() >= SYNTAX_BREAKER_WINDOW {
                window.pop_front();
            }
            window.push_back(valid);
            window.iter().filter(|&&ok| !ok).count()
        };
        if failures >= SYNTAX_BREAKER_MIN_FAILURES {
            state.open_until.insert(
                language.to_string(),
                Instant::now() + Duration::from_secs(SYNTAX_BREAKER_COOLDOWN_SECS),
            );
            *state.trips.entry(language.to_string()).or_insert(0) += 1;
            tripped_skips = state
                .invalid_input_skips
                .get(language)
                .copied()
                .unwrap_or(0);
            // A fresh window after the cooldown must re-earn the next
            // pause. Skips are kept: they describe the traffic mix, not
            // this window, and resetting them would hide a
            // fragments-dominated stream.
            if let Some(window) = state.outcomes.get_mut(language) {
                window.clear();
            }
            tripped_failures = failures;
        }
    }
    if tripped_failures > 0 {
        tracing::warn!(
            event = "code_syntax_breaker_tripped",
            language = language,
            invalid_input_skipped = tripped_skips,
            "Code compression for {language} paused for {} min: {tripped_failures} of the last {SYNTAX_BREAKER_WINDOW} attempts produced invalid syntax and were discarded (HEADROOM_CODE_SYNTAX_BREAKER=0 disables this breaker)",
            SYNTAX_BREAKER_COOLDOWN_SECS / 60,
        );
    }
}

/// A fragment that never parsed is input noise, not a compressor failure:
/// counted separately so the trip line can tell "broken grammar" apart from
/// "mostly fragments". Never touches the failure window.
pub(super) fn record_syntax_input_skipped(language: &str) {
    if !syntax_breaker_enabled() {
        return;
    }
    let mut state = syntax_breaker_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *state
        .invalid_input_skips
        .entry(language.to_string())
        .or_insert(0) += 1;
}

/// Per-language breaker state for one language, surfaced on `/stats`.
///
/// While the breaker is open that language compresses at ratio 1.0, so
/// savings drop with nothing in the payload explaining why. The map is empty
/// until something trips, so a healthy install carries no extra keys.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SyntaxBreakerLanguageStatus {
    pub open: bool,
    pub trips: u64,
    pub reopens_in_seconds: f64,
}

/// Per-language breaker state keyed by language value, sorted. Empty until
/// something trips.
pub fn syntax_breaker_status() -> BTreeMap<String, SyntaxBreakerLanguageStatus> {
    let now = Instant::now();
    let state = syntax_breaker_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut out = BTreeMap::new();
    let mut languages: Vec<&String> = state.trips.keys().chain(state.open_until.keys()).collect();
    languages.sort();
    languages.dedup();
    for language in languages {
        let until = state.open_until.get(language);
        let open = until.is_some_and(|t| *t > now);
        let remaining = until
            .and_then(|t| t.checked_duration_since(now))
            .map(|d| (d.as_secs_f64() * 10.0).round() / 10.0)
            .unwrap_or(0.0);
        out.insert(
            language.clone(),
            SyntaxBreakerLanguageStatus {
                open,
                trips: state.trips.get(language).copied().unwrap_or(0),
                reopens_in_seconds: remaining,
            },
        );
    }
    out
}
