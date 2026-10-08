//! Request-scoped ML compression budget (issue #3711).
//!
//! The Kompress size gate bounds one block, but a request is many blocks: a
//! 1.4MB tool result of prose sections separated by small JSON objects spent
//! ~70s in the ML stage while every block passed the per-block gate. This
//! module ports the upstream fix: a wall-clock ceiling armed per request that
//! the ML stage draws down, so past the ceiling the remaining blocks pass
//! through unchanged instead of compounding.
//!
//! [`MlBudget`] is the shared handle; the live-zone entry points arm one per
//! request and carry it on [`DispatchConfig`](super::DispatchConfig). Dispatch
//! checks admission before each ML call and charges the call's measured time
//! after it, including failed calls (otherwise a request could retry past the
//! ceiling). Admission uses the worst observed secs-per-token rate, so a call
//! starting near the deadline is refused when the projection says it cannot
//! finish — and the surviving remainder reaches Kompress as a per-call cap,
//! checked at chunk boundaries, so a projection that guessed low cannot
//! overrun either.
//!
//! Refused blocks pass through with their bytes untouched (never a lossy
//! fallback): the refusal depends on the request, not the content, and the
//! dispatch cache memoizes the verdict, so raw bytes are the only ones that
//! stay stable on later turns.
//!
//! Deliberately not ported: the clamp under the quarantining executor timeout
//! (Rust has no compression quarantine), the shared `deadline_started_at`
//! (no outer compression timeout on this path), and the remote 16-worker pool
//! (no production transport implements `KompressTransport` yet).

use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `HEADROOM_ML_STAGE_DEADLINE_SECONDS` default: sits under the 30s
/// compression timeout so requests degrade to partial compression before an
/// executor would abandon a non-preemptible worker.
pub const DEFAULT_ML_STAGE_DEADLINE_SECONDS: f64 = 15.0;

/// Env var arming the per-request ML budget; `<= 0` restores unbounded.
pub const ML_STAGE_DEADLINE_ENV: &str = "HEADROOM_ML_STAGE_DEADLINE_SECONDS";

/// A call admitted with less than this is refused: it could not finish one
/// chunk, so admitting it would only burn the remainder for nothing.
pub const MIN_USEFUL_CHUNK_SECONDS: f64 = 0.05;

/// Chars-per-token estimate for the admission projection. Matches the size
/// gate's estimator (`kompress_size_gate_exceeded`), so the two agree on a
/// block's size.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.len() / 4) as u64
}

/// Parse the ceiling the way Python's `_env_float` + default does: missing,
/// blank, or unparseable reads as the 15s default (with a warning for the
/// unparseable case, matching `HEADROOM_COMPRESS_WORKERS`' fall-back).
pub fn ml_stage_deadline_seconds_from(raw: Option<&str>) -> f64 {
    match raw {
        None => DEFAULT_ML_STAGE_DEADLINE_SECONDS,
        Some(s) if s.trim().is_empty() => DEFAULT_ML_STAGE_DEADLINE_SECONDS,
        Some(s) => match s.trim().parse::<f64>() {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(
                    "{} must be a number, got {:?}; using default {}",
                    ML_STAGE_DEADLINE_ENV,
                    s,
                    DEFAULT_ML_STAGE_DEADLINE_SECONDS
                );
                DEFAULT_ML_STAGE_DEADLINE_SECONDS
            }
        },
    }
}

/// Per-request ML time ledger. Lives behind [`MlBudget`]'s mutex; all methods
/// assume the caller holds the lock for the shortest possible span (plain
/// float ops, never blocking).
#[derive(Debug)]
pub struct RequestMlBudget {
    ceiling_secs: f64,
    elapsed_secs: f64,
    worst_secs_per_token: f64,
    warned: bool,
    warn_emitted: bool,
}

impl RequestMlBudget {
    /// Fresh ledger with a `ceiling_secs` ceiling. Only meaningful for
    /// positive ceilings; [`MlBudget::armed`] enforces that.
    pub fn with_ceiling(ceiling_secs: f64) -> Self {
        Self {
            ceiling_secs,
            elapsed_secs: 0.0,
            worst_secs_per_token: 0.0,
            warned: false,
            warn_emitted: false,
        }
    }

    /// Time left in the budget. Never negative.
    pub fn remaining_secs(&self) -> f64 {
        (self.ceiling_secs - self.elapsed_secs).max(0.0)
    }

    /// Admit a block of `est_tokens`, returning the remainder as Kompress's
    /// per-call cap — or refuse, with the warn-once flag set so the caller
    /// logs `ml_stage_budget_exceeded` exactly once per request.
    ///
    /// Refusal is not just "nothing left": when earlier calls set a worst
    /// rate, a block whose projection overruns the remainder is refused even
    /// with time on the clock, and a remainder under one chunk's worth is
    /// refused outright.
    pub fn admit(&mut self, est_tokens: u64) -> Option<f64> {
        let remaining = self.remaining_secs();
        if remaining <= 0.0 {
            self.warned = true;
            return None;
        }
        if self.worst_secs_per_token > 0.0
            && est_tokens as f64 * self.worst_secs_per_token > remaining
        {
            self.warned = true;
            return None;
        }
        if remaining < MIN_USEFUL_CHUNK_SECONDS {
            self.warned = true;
            return None;
        }
        Some(remaining)
    }

    /// Charge a finished ML call. `charge` runs for failed calls too — the
    /// caller measures wall time around the call and reports it
    /// unconditionally.
    pub fn charge(&mut self, elapsed: Duration, est_tokens: u64) {
        let secs = elapsed.as_secs_f64();
        self.elapsed_secs += secs;
        if est_tokens > 0 {
            self.worst_secs_per_token = self.worst_secs_per_token.max(secs / est_tokens as f64);
        }
    }

    /// True once any call has been refused. The caller emits the
    /// `ml_stage_budget_exceeded` warning on the false→true edge.
    pub fn warned(&self) -> bool {
        self.warned
    }

    /// Take the warn-once token: true exactly once per ledger. The caller
    /// invokes this in the refused branch, after [`admit`](Self::admit)
    /// already flagged the refusal, and logs only on true.
    pub fn take_warn(&mut self) -> bool {
        if self.warn_emitted {
            false
        } else {
            self.warn_emitted = true;
            true
        }
    }

    #[cfg(test)]
    pub fn elapsed_secs(&self) -> f64 {
        self.elapsed_secs
    }

    #[cfg(test)]
    pub fn worst_secs_per_token(&self) -> f64 {
        self.worst_secs_per_token
    }
}

/// Outcome of asking a request's [`MlBudget`] to run one ML call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Admission {
    /// No ledger (unbounded request, or accounting failed): run uncapped.
    Unbounded,
    /// Run with `cap_secs` as Kompress's per-call cap.
    Admitted { cap_secs: f64 },
    /// Budget spent: pass the block through unchanged.
    Refused,
}

/// Shareable handle to a request's [`RequestMlBudget`]. `Clone` shares the
/// ledger; dispatch is serial today, the mutex is for a parallel tomorrow.
#[derive(Debug, Clone, Default)]
pub struct MlBudget {
    inner: Option<Arc<Mutex<RequestMlBudget>>>,
}

impl MlBudget {
    /// Arm a fresh budget from the environment. `None` when the ceiling is
    /// `<= 0`: unbounded, the pre-#3711 behavior. Read once per request at
    /// the entry points — Python re-reads per block, but a ceiling that can
    /// flip mid-request is a bug, not a feature.
    pub fn armed() -> Option<Self> {
        let raw = std::env::var(ML_STAGE_DEADLINE_ENV).ok();
        let ceiling = ml_stage_deadline_seconds_from(raw.as_deref());
        // A non-finite ceiling admits everything and then panics turning the
        // infinite cap into a `Duration`. Python treats it as unbounded (an
        // infinite deadline never fires), so do we.
        if ceiling > 0.0 && ceiling.is_finite() {
            Some(Self::with_ceiling(ceiling))
        } else {
            None
        }
    }

    /// Deterministic constructor for tests and callers that already resolved
    /// the ceiling. Only meaningful for positive, finite ceilings; anything
    /// else should stay `None` (unbounded) at the call site.
    pub fn with_ceiling(ceiling_secs: f64) -> Self {
        Self {
            inner: Some(Arc::new(Mutex::new(RequestMlBudget::with_ceiling(
                ceiling_secs,
            )))),
        }
    }

    /// Admit-or-refuse a block. A poisoned or missing ledger fails open
    /// (uncapped): accounting must never break compression, matching
    /// Python's defensive `try/except` around the same bookkeeping.
    pub fn admit(&self, est_tokens: u64) -> Admission {
        let Some(inner) = self.inner.as_ref() else {
            return Admission::Unbounded;
        };
        let Ok(mut ledger) = inner.lock() else {
            return Admission::Unbounded;
        };
        match ledger.admit(est_tokens) {
            Some(cap_secs) => Admission::Admitted { cap_secs },
            None => Admission::Refused,
        }
    }

    /// Charge a finished call; see [`RequestMlBudget::charge`].
    pub fn charge(&self, elapsed: Duration, est_tokens: u64) {
        if let Some(inner) = self.inner.as_ref()
            && let Ok(mut ledger) = inner.lock()
        {
            ledger.charge(elapsed, est_tokens);
        }
    }

    /// Remaining budget in seconds, for the per-call cap deadline.
    pub fn remaining_secs(&self) -> Option<f64> {
        let inner = self.inner.as_ref()?;
        inner.lock().ok().map(|ledger| ledger.remaining_secs())
    }

    /// True once any call has been refused this request.
    pub fn warned(&self) -> bool {
        self.inner
            .as_ref()
            .and_then(|inner| inner.lock().ok().map(|ledger| ledger.warned()))
            .unwrap_or(false)
    }

    /// Take the warn-once token; see [`RequestMlBudget::take_warn`]. A
    /// poisoned or missing ledger fails open (no warning).
    pub fn take_warn(&self) -> bool {
        self.inner
            .as_ref()
            .and_then(|inner| inner.lock().ok().map(|mut ledger| ledger.take_warn()))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_parse_matches_python() {
        assert_eq!(ml_stage_deadline_seconds_from(None), 15.0);
        assert_eq!(ml_stage_deadline_seconds_from(Some("")), 15.0);
        assert_eq!(ml_stage_deadline_seconds_from(Some("   ")), 15.0);
        assert_eq!(ml_stage_deadline_seconds_from(Some("0.5")), 0.5);
        assert_eq!(ml_stage_deadline_seconds_from(Some("0")), 0.0);
        assert_eq!(ml_stage_deadline_seconds_from(Some("-3")), -3.0);
        // Unparseable falls back to the default (warns, like compress workers).
        assert_eq!(ml_stage_deadline_seconds_from(Some("soon")), 15.0);
    }

    #[test]
    fn spent_budget_refuses_further_calls() {
        let budget = MlBudget::with_ceiling(0.06);
        assert!(matches!(budget.admit(100), Admission::Admitted { .. }));
        budget.charge(Duration::from_millis(60), 100);
        assert_eq!(budget.admit(100), Admission::Refused);
        assert!(budget.warned());
    }

    #[test]
    fn take_warn_fires_once_per_request() {
        let budget = MlBudget::with_ceiling(0.06);
        budget.charge(Duration::from_millis(60), 100);
        assert_eq!(budget.admit(100), Admission::Refused);
        assert!(budget.take_warn());
        assert!(!budget.take_warn());
    }

    #[test]
    fn worst_rate_refuses_a_call_it_will_overrun() {
        let budget = MlBudget::with_ceiling(0.5);
        assert!(matches!(budget.admit(100), Admission::Admitted { .. }));
        // One slow call: 0.4s for 100 tokens → 4ms/token worst rate.
        budget.charge(Duration::from_millis(400), 100);
        // 0.1s left; a 100-token block at the worst rate needs 0.4s.
        assert_eq!(budget.admit(100), Admission::Refused);
        // A 10-token block at the worst rate needs 0.04s: admitted.
        assert!(matches!(budget.admit(10), Admission::Admitted { .. }));
    }

    #[test]
    fn sub_chunk_remainder_is_refused() {
        let budget = MlBudget::with_ceiling(1.0);
        budget.charge(Duration::from_millis(990), 10);
        // 10ms left: less than one chunk's worth.
        assert_eq!(budget.admit(10), Admission::Refused);
    }

    #[test]
    fn charge_accumulates_elapsed_and_worst_rate() {
        let mut ledger = RequestMlBudget::with_ceiling(10.0);
        ledger.charge(Duration::from_millis(100), 100);
        ledger.charge(Duration::from_millis(400), 100);
        assert!((ledger.elapsed_secs() - 0.5).abs() < 1e-9);
        assert!((ledger.worst_secs_per_token() - 0.004).abs() < 1e-9);
        // Zero-token charges add elapsed but cannot set a rate.
        ledger.charge(Duration::from_secs(1), 0);
        assert!((ledger.worst_secs_per_token() - 0.004).abs() < 1e-9);
    }

    #[test]
    fn admission_returns_the_remainder_as_cap() {
        let budget = MlBudget::with_ceiling(1.0);
        budget.charge(Duration::from_millis(250), 100);
        let Admission::Admitted { cap_secs } = budget.admit(10) else {
            panic!("expected admission");
        };
        assert!((cap_secs - 0.75).abs() < 1e-9);
    }

    #[test]
    fn unarmed_budget_fails_open() {
        let budget = MlBudget::default();
        assert_eq!(budget.admit(u64::MAX), Admission::Unbounded);
        assert_eq!(budget.remaining_secs(), None);
        assert!(!budget.warned());
        // Charging a missing ledger is a silent no-op, not a panic.
        budget.charge(Duration::from_secs(60), 100);
    }

    #[test]
    fn non_finite_ceiling_means_unbounded() {
        // `Duration::from_secs_f64(inf)` panics, so an infinite ceiling must
        // never arm: Python treats it as unbounded and so do we. Serial-test
        // guard: this owns the process env for the ceiling variable.
        let key = ML_STAGE_DEADLINE_ENV;
        let prev = std::env::var(key).ok();
        for raw in ["inf", "-inf", "NaN"] {
            unsafe { std::env::set_var(key, raw) };
            assert!(MlBudget::armed().is_none(), "{raw} must not arm a budget");
        }
        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
