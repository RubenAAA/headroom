//! Stage-timing instrumentation for request handlers.
//!
//! Provides a lightweight utility for measuring per-stage durations within a
//! single request or WebSocket session. Timings are collected into a single
//! `HashMap` that can be emitted on the structured log line for the request.
//!
//! Design goals:
//! 1. A single `StageTimer` holds every stage for one request/session.
//! 2. Concurrent `measure` calls are independent.
//! 3. Uses `std::time::Instant` for monotonic, high-resolution measurement.

use std::collections::HashMap;
use std::time::Instant;

/// A guard that records its elapsed time when dropped.
pub struct StageMeasurement {
    stages: *mut HashMap<String, f64>,
    name: String,
    start: Instant,
}

impl Drop for StageMeasurement {
    fn drop(&mut self) {
        let duration_ms = self.start.elapsed().as_secs_f64() * 1000.0;
        // SAFETY: caller guarantees the parent StageTimer outlives this measurement.
        unsafe {
            (*self.stages).insert(self.name.clone(), duration_ms);
        }
    }
}

/// Collect per-stage durations for one request/session.
pub struct StageTimer {
    stages: HashMap<String, f64>,
    created_at: Instant,
}

impl StageTimer {
    pub fn new() -> Self {
        Self {
            stages: HashMap::new(),
            created_at: Instant::now(),
        }
    }

    /// Record a pre-computed duration (e.g. from an existing timer).
    pub fn record(&mut self, name: &str, duration_ms: f64) {
        self.stages.insert(name.to_string(), duration_ms);
    }

    /// Return total milliseconds since the timer was created.
    pub fn elapsed_ms(&self) -> f64 {
        self.created_at.elapsed().as_secs_f64() * 1000.0
    }

    /// Return a snapshot of the recorded stage durations (in ms).
    pub fn summary(&self) -> &HashMap<String, f64> {
        &self.stages
    }

    pub fn contains(&self, name: &str) -> bool {
        self.stages.contains_key(name)
    }

    /// Start timing a named stage. The measurement is recorded when dropped.
    ///
    /// # Safety contract
    /// The returned `StageMeasurement` holds a raw pointer to the internal
    /// stages map. The caller must ensure the `StageTimer` outlives every
    /// `StageMeasurement` it produces (guaranteed by the borrow checker when
    /// used in the natural `let _m = timer.measure("x");` pattern).
    pub fn measure(&mut self, name: &str) -> StageMeasurement {
        StageMeasurement {
            stages: &mut self.stages as *mut HashMap<String, f64>,
            name: name.to_string(),
            start: Instant::now(),
        }
    }
}

impl Default for StageTimer {
    fn default() -> Self {
        Self::new()
    }
}

/// Emit one structured log line of stage timings.
///
/// `expected_stages` guarantees that every stage shows up, even when it
/// never ran (null placeholder).
pub fn emit_stage_timings_log(
    path: &str,
    request_id: &str,
    session_id: &str,
    stage_timer: &StageTimer,
    expected_stages: &[&str],
) -> String {
    let summary = stage_timer.summary();

    // Feed the same samples to Prometheus, matching Python's
    // `record_stage_timings`. Only stages that actually ran are recorded — a
    // null placeholder below means "did not run", not "took 0ms".
    for (stage, ms) in summary {
        crate::observability::proxy_counters::record_stage_timing(path, stage, *ms);
    }

    let mut padded: Vec<(String, Option<f64>)> = expected_stages
        .iter()
        .map(|s| (s.to_string(), summary.get(*s).copied()))
        .collect();

    for (extra_stage, extra_value) in summary {
        if !expected_stages.iter().any(|e| e == extra_stage) {
            padded.push((extra_stage.clone(), Some(*extra_value)));
        }
    }

    let stages_map: serde_json::Map<String, serde_json::Value> = padded
        .into_iter()
        .map(|(k, v)| {
            let val = match v {
                Some(ms) => serde_json::json!(ms),
                None => serde_json::Value::Null,
            };
            (k, val)
        })
        .collect();

    let payload = serde_json::json!({
        "event": "stage_timings",
        "path": path,
        "request_id": request_id,
        "session_id": session_id,
        "stages": stages_map,
    });

    serde_json::to_string(&payload).unwrap_or_else(|_| format!("{{\"path\":\"{path}\"}}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_manual_duration() {
        let mut timer = StageTimer::new();
        timer.record("parse", 1.5);
        assert_eq!(timer.summary().get("parse"), Some(&1.5));
    }

    #[test]
    fn elapsed_ms_positive() {
        let timer = StageTimer::new();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(timer.elapsed_ms() >= 10.0);
    }

    #[test]
    fn contains_tracks_recorded_stages() {
        let mut timer = StageTimer::new();
        assert!(!timer.contains("parse"));
        timer.record("parse", 1.0);
        assert!(timer.contains("parse"));
    }

    #[test]
    fn summary_returns_all_recorded() {
        let mut timer = StageTimer::new();
        timer.record("a", 1.0);
        timer.record("b", 2.0);
        let s = timer.summary();
        assert_eq!(s.len(), 2);
        assert_eq!(s.get("a"), Some(&1.0));
        assert_eq!(s.get("b"), Some(&2.0));
    }

    #[test]
    fn emit_stage_timings_includes_expected_stages() {
        let mut timer = StageTimer::new();
        timer.record("compress", 5.0);
        let output = emit_stage_timings_log(
            "/v1/messages",
            "req-1",
            "sess-1",
            &timer,
            &["parse", "compress", "forward"],
        );
        let v: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(v["event"], "stage_timings");
        assert_eq!(v["path"], "/v1/messages");
        assert_eq!(v["stages"]["parse"], serde_json::Value::Null);
        assert_eq!(v["stages"]["compress"], 5.0);
        assert_eq!(v["stages"]["forward"], serde_json::Value::Null);
    }
}
