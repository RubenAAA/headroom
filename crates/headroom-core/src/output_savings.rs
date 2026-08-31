//! Counterfactual estimation of output-token reduction (Rust port of
//! `headroom/proxy/output_savings.py`).
//!
//! Output-token savings are counterfactual: we observe the shaped output but
//! never the unshaped one. This module keeps the estimate honest by separating
//! three tiers — an offline synthetic-control baseline ("estimated"), an A/B
//! holdout difference ("measured"), and echo-ratio direct-waste (no
//! counterfactual). Stratification uses only request-time features.
//!
//! Deviation: Python's `process_is_stateless()` also honours a settable process
//! global; Rust checks only the `HEADROOM_STATELESS` env var for the flush
//! guard (the proxy sets that env for stateless deploys).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Coarse input-token bucket boundaries (tokens).
const INPUT_BUCKETS: [i64; 4] = [2_000, 8_000, 32_000, 128_000];

/// Map an input-token count to a coarse bucket label.
pub fn input_bucket(input_tokens: i64) -> &'static str {
    if input_tokens < INPUT_BUCKETS[0] {
        "xs"
    } else if input_tokens < INPUT_BUCKETS[1] {
        "s"
    } else if input_tokens < INPUT_BUCKETS[2] {
        "m"
    } else if input_tokens < INPUT_BUCKETS[3] {
        "l"
    } else {
        "xl"
    }
}

/// Collapse a model id to a coarse family for stratification.
pub fn model_family(model: &str) -> &'static str {
    let m = model.to_lowercase();
    for fam in [
        "opus", "sonnet", "haiku", "fable", "mythos", "gpt", "gemini",
    ] {
        if m.contains(fam) {
            return fam;
        }
    }
    "other"
}

/// Build a stratum key from request features observable BEFORE the response.
/// Order is most→least specific so [`BaselineModel::lookup`] can back off.
pub fn stratum_key(turn_kind: &str, input_tokens: i64, model: &str, has_tools: bool) -> String {
    format!(
        "{}|{}|{}|{}",
        model_family(model),
        turn_kind,
        input_bucket(input_tokens),
        if has_tools { "tools" } else { "notools" }
    )
}

/// Take the first `n` characters (code points) of `s`, matching Python slicing.
fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Derive a conversation-stable key (model + first user message text) for
/// holdout assignment.
pub fn conversation_key_from_body(body: &serde_json::Value) -> String {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut seed = model;
    if let Some(msgs) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in msgs {
            if msg.get("role").and_then(|v| v.as_str()) != Some("user") {
                continue;
            }
            match msg.get("content") {
                Some(serde_json::Value::String(s)) => {
                    seed.push('\u{0}');
                    seed.push_str(&first_chars(s, 512));
                }
                Some(serde_json::Value::Array(blocks)) => {
                    for block in blocks {
                        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                            let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            seed.push('\u{0}');
                            seed.push_str(&first_chars(text, 512));
                            break;
                        }
                    }
                }
                _ => {}
            }
            break;
        }
    }
    let digest = Sha256::digest(seed.as_bytes());
    hex::encode(digest)
}

/// Deterministically assign a conversation to `treatment` or `control`.
/// `holdout_fraction` in [0, 1] is the share routed to `control`.
pub fn assign_arm(conversation_key: &str, holdout_fraction: f64) -> &'static str {
    if holdout_fraction <= 0.0 {
        return "treatment";
    }
    if holdout_fraction >= 1.0 {
        return "control";
    }
    let digest = hex::encode(Sha256::digest(format!("arm:{conversation_key}").as_bytes()));
    let first8 = u32::from_str_radix(&digest[..8], 16).unwrap_or(0);
    let frac = first8 as f64 / 0xFFFF_FFFFu32 as f64;
    if frac < holdout_fraction {
        "control"
    } else {
        "treatment"
    }
}

/// Running count / sum / sum-of-squares for online mean & variance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Accum {
    #[serde(default)]
    pub n: i64,
    #[serde(default)]
    pub sum: f64,
    #[serde(default)]
    pub sumsq: f64,
}

impl Accum {
    pub fn add(&mut self, x: f64) {
        self.n += 1;
        self.sum += x;
        self.sumsq += x * x;
    }

    pub fn mean(&self) -> f64 {
        if self.n != 0 {
            self.sum / self.n as f64
        } else {
            0.0
        }
    }

    /// Sample (unbiased) variance; 0 when fewer than 2 observations.
    pub fn var(&self) -> f64 {
        if self.n < 2 {
            return 0.0;
        }
        let n = self.n as f64;
        ((self.sumsq - self.sum * self.sum / n) / (n - 1.0)).max(0.0)
    }

    /// Fold another accumulator's observations in (element-wise addition).
    pub fn merge(&mut self, other: &Accum) {
        self.n += other.n;
        self.sum += other.sum;
        self.sumsq += other.sumsq;
    }
}

/// Per-stratum baseline of unshaped output tokens (the synthetic control).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaselineModel {
    #[serde(default)]
    pub strata: HashMap<String, Accum>,
    #[serde(default)]
    pub glob: Accum,
}

impl BaselineModel {
    pub fn observe(&mut self, key: &str, output_tokens: i64) {
        self.strata
            .entry(key.to_string())
            .or_default()
            .add(output_tokens as f64);
        self.glob.add(output_tokens as f64);
    }

    pub fn merge(&mut self, other: &BaselineModel) {
        for (key, acc) in &other.strata {
            self.strata.entry(key.clone()).or_default().merge(acc);
        }
        self.glob.merge(&other.glob);
    }

    /// Return `(mean, var, n)` for `key` with hierarchical back-off: trim
    /// trailing stratum fields, then fall back to the global mean.
    pub fn lookup(&self, key: &str) -> (f64, f64, i64) {
        if let Some(acc) = self.strata.get(key) {
            if acc.n > 0 {
                return (acc.mean(), acc.var(), acc.n);
            }
        }
        let mut parts: Vec<&str> = key.split('|').collect();
        while parts.len() > 1 {
            parts.pop();
            let prefix = format!("{}|", parts.join("|"));
            for (k, a) in &self.strata {
                if k.starts_with(&prefix) && a.n > 0 {
                    return (a.mean(), a.var(), a.n);
                }
            }
        }
        (self.glob.mean(), self.glob.var(), self.glob.n)
    }

    pub fn total_samples(&self) -> i64 {
        self.glob.n
    }
}

/// Result of an estimation pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavingsEstimate {
    pub tokens_saved: f64,
    pub baseline_tokens: f64,
    pub pct: f64,
    pub ci_low_pct: f64,
    pub ci_high_pct: f64,
    pub n_requests: i64,
    /// "estimated" (synthetic control) or "measured" (A/B holdout).
    pub kind: String,
}

/// Accumulates shaped (treatment) and unshaped (control) observations and
/// produces honest reduction estimates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavingsLedger {
    #[serde(default)]
    pub baseline: BaselineModel,
    #[serde(default)]
    pub treatment: HashMap<String, Accum>,
    #[serde(default)]
    pub control: HashMap<String, Accum>,
}

impl SavingsLedger {
    pub fn record(&mut self, arm: &str, key: &str, output_tokens: i64) {
        let target = if arm == "treatment" {
            &mut self.treatment
        } else {
            &mut self.control
        };
        target
            .entry(key.to_string())
            .or_default()
            .add(output_tokens as f64);
    }

    /// Synthetic-control estimate: treatment output vs. offline baseline.
    pub fn estimate_from_baseline(&self) -> SavingsEstimate {
        let mut total_saved = 0.0;
        let mut total_baseline = 0.0;
        let mut var = 0.0;
        let mut n_requests = 0i64;
        for (key, acc) in &self.treatment {
            if acc.n == 0 {
                continue;
            }
            let (mu, mu_var, m) = self.baseline.lookup(key);
            if m == 0 {
                continue;
            }
            let n = acc.n;
            n_requests += n;
            total_saved += n as f64 * (mu - acc.mean());
            total_baseline += n as f64 * mu;
            var += n as f64 * acc.var();
            var += (n as f64 * n as f64) * (mu_var / m as f64);
        }
        Self::finalize(total_saved, total_baseline, var, n_requests, "estimated")
    }

    /// A/B measurement: per-stratum control mean minus treatment mean. `None`
    /// when no stratum has data in both arms.
    pub fn estimate_from_holdout(&self) -> Option<SavingsEstimate> {
        let mut total_saved = 0.0;
        let mut total_baseline = 0.0;
        let mut var = 0.0;
        let mut n_requests = 0i64;
        let mut contributing = 0;
        for (key, t) in &self.treatment {
            let Some(c) = self.control.get(key) else {
                continue;
            };
            if c.n == 0 || t.n == 0 {
                continue;
            }
            contributing += 1;
            let n = t.n;
            n_requests += n;
            let delta = c.mean() - t.mean();
            total_saved += n as f64 * delta;
            total_baseline += n as f64 * c.mean();
            var += (n as f64 * n as f64) * (c.var() / c.n as f64 + t.var() / t.n as f64);
        }
        if contributing == 0 {
            return None;
        }
        Some(Self::finalize(
            total_saved,
            total_baseline,
            var,
            n_requests,
            "measured",
        ))
    }

    fn finalize(
        total_saved: f64,
        total_baseline: f64,
        var: f64,
        n_requests: i64,
        kind: &str,
    ) -> SavingsEstimate {
        let pct = if total_baseline > 0.0 {
            total_saved / total_baseline * 100.0
        } else {
            0.0
        };
        let se = var.sqrt();
        let lo = total_saved - 1.96 * se;
        let hi = total_saved + 1.96 * se;
        let (ci_low, ci_high) = if total_baseline > 0.0 {
            (lo / total_baseline * 100.0, hi / total_baseline * 100.0)
        } else {
            (0.0, 0.0)
        };
        SavingsEstimate {
            tokens_saved: total_saved,
            baseline_tokens: total_baseline,
            pct,
            ci_low_pct: ci_low,
            ci_high_pct: ci_high,
            n_requests,
            kind: kind.to_string(),
        }
    }

    /// Prefer the measured A/B number; fall back to the baseline estimate.
    pub fn best_estimate(&self) -> SavingsEstimate {
        self.estimate_from_holdout()
            .unwrap_or_else(|| self.estimate_from_baseline())
    }

    /// Write the ledger, leaving either the old file or the new one behind.
    ///
    /// A plain `write` truncates first, so a crash or a concurrent reader
    /// mid-write sees a half-written file — and `load` treats unparseable JSON
    /// as an empty ledger, so the savings history would silently reset to
    /// zero. Writing to a temporary beside the target and renaming makes the
    /// swap atomic; the pid and process-local sequence in the suffix keep
    /// concurrent writers from sharing a temporary. Same shape as
    /// `subscription::tracker`.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let sequence = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp.{}.{sequence}", std::process::id()));
        let result = (|| {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Load from disk, returning a fresh empty ledger on missing/corrupt file.
    pub fn load(path: &Path) -> SavingsLedger {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return SavingsLedger::default();
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "output-savings ledger unreadable; starting empty"
                );
                return SavingsLedger::default();
            }
        };
        match serde_json::from_str(&text) {
            Ok(ledger) => ledger,
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "output-savings ledger corrupt; starting empty"
                );
                SavingsLedger::default()
            }
        }
    }
}

// ── Live recording via the transforms_applied label channel ──

const STRATUM_LABEL: &str = "output_shaper:stratum:";
const CONTROL_LABEL: &str = "output_shaper:control:";

/// Encode (arm, stratum) as a transforms_applied label.
pub fn stratum_label(arm: &str, key: &str) -> String {
    let prefix = if arm == "treatment" {
        STRATUM_LABEL
    } else {
        CONTROL_LABEL
    };
    format!("{prefix}{key}")
}

/// Decode a label into `(arm, stratum)`, or `None` if not one of ours.
pub fn parse_stratum_label(label: &str) -> Option<(&'static str, String)> {
    if let Some(rest) = label.strip_prefix(STRATUM_LABEL) {
        return Some(("treatment", rest.to_string()));
    }
    if let Some(rest) = label.strip_prefix(CONTROL_LABEL) {
        return Some(("control", rest.to_string()));
    }
    None
}

fn process_is_stateless() -> bool {
    std::env::var("HEADROOM_STATELESS")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// In-memory ledger with periodic flush, safe for concurrent requests.
pub struct SavingsRecorder {
    path: PathBuf,
    flush_every: u32,
    state: Mutex<RecorderState>,
}

struct RecorderState {
    ledger: SavingsLedger,
    since_flush: u32,
}

impl SavingsRecorder {
    pub fn new(path: PathBuf, flush_every: u32) -> Self {
        let ledger = SavingsLedger::load(&path);
        Self {
            path,
            flush_every,
            state: Mutex::new(RecorderState {
                ledger,
                since_flush: 0,
            }),
        }
    }

    /// Record one outcome given its transforms_applied labels. Returns `true` if
    /// a shaping label was found and recorded.
    pub fn record_from_labels(&self, labels: &[String], output_tokens: i64) -> bool {
        for label in labels {
            let Some((arm, key)) = parse_stratum_label(label) else {
                continue;
            };
            let mut st = self.state.lock().unwrap();
            st.ledger.record(arm, &key, output_tokens);
            st.since_flush += 1;
            if st.since_flush >= self.flush_every {
                self.flush_locked(&mut st);
            }
            return true;
        }
        false
    }

    /// Per-request output tokens saved, for the savings rollup.
    ///
    /// For a treatment request, the synthetic-control estimate
    /// `max(0, baseline_mean(stratum) - output_tokens)`; 0 for control, unknown
    /// strata, or when no shaping label is present.
    ///
    /// Read-only: unlike [`Self::record_from_labels`] it does not mutate the
    /// ledger, so the two compose without double-counting.
    pub fn estimate_request_savings(&self, labels: &[String], output_tokens: i64) -> i64 {
        for label in labels {
            let Some((arm, key)) = parse_stratum_label(label) else {
                continue;
            };
            if arm != "treatment" {
                return 0;
            }
            let (mean, _var, n) = {
                let st = self.state.lock().unwrap();
                st.ledger.baseline.lookup(&key)
            };
            if n <= 0 {
                return 0;
            }
            // `round_ties_even` matches Python's `round()`, which is
            // banker's rounding — plain `.round()` would differ on exact .5.
            return ((mean - output_tokens as f64).round_ties_even() as i64).max(0);
        }
        0
    }

    /// Adopt the on-disk baseline (written by `learn --verbosity --apply`) when
    /// it carries samples and differs from ours.
    fn reload_baseline_locked(&self, st: &mut RecorderState) {
        let disk = SavingsLedger::load(&self.path);
        if disk.baseline.total_samples() == 0 {
            return;
        }
        // Compare by serialized content, matching Python's to_dict() != to_dict().
        let disk_json = serde_json::to_string(&disk.baseline).unwrap_or_default();
        let ours_json = serde_json::to_string(&st.ledger.baseline).unwrap_or_default();
        if disk_json != ours_json {
            st.ledger.baseline = disk.baseline;
        }
    }

    fn flush_locked(&self, st: &mut RecorderState) {
        if process_is_stateless() {
            st.since_flush = 0;
            return;
        }
        self.reload_baseline_locked(st);
        if st.ledger.save(&self.path).is_ok() {
            st.since_flush = 0;
        }
    }

    pub fn flush(&self) {
        let mut st = self.state.lock().unwrap();
        self.flush_locked(&mut st);
    }

    pub fn estimate(&self) -> SavingsEstimate {
        let mut st = self.state.lock().unwrap();
        self.reload_baseline_locked(&mut st);
        st.ledger.best_estimate()
    }
}

static RECORDER: OnceLock<SavingsRecorder> = OnceLock::new();

/// Process-wide recorder singleton, rooted at the workspace dir.
pub fn get_recorder() -> &'static SavingsRecorder {
    RECORDER.get_or_init(|| {
        let path = crate::paths::workspace_dir().join("output_savings.json");
        SavingsRecorder::new(path, 25)
    })
}

/// Fraction of the response's n-grams that already appear in the context.
/// Returns 0.0 when the output is shorter than `n`.
pub fn echo_ratio(output_text: &str, context_text: &str, n: usize) -> f64 {
    let out_words: Vec<&str> = output_text.split_whitespace().collect();
    if out_words.len() < n {
        return 0.0;
    }
    let ctx_words: Vec<&str> = context_text.split_whitespace().collect();
    let ctx_grams: std::collections::HashSet<String> = if ctx_words.len() >= n {
        (0..=ctx_words.len() - n)
            .map(|i| ctx_words[i..i + n].join(" "))
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    if ctx_grams.is_empty() {
        return 0.0;
    }
    let out_grams: Vec<String> = (0..=out_words.len() - n)
        .map(|i| out_words[i..i + n].join(" "))
        .collect();
    if out_grams.is_empty() {
        return 0.0;
    }
    let hits = out_grams.iter().filter(|g| ctx_grams.contains(*g)).count();
    hits as f64 / out_grams.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn input_bucket_boundaries() {
        assert_eq!(input_bucket(0), "xs");
        assert_eq!(input_bucket(1_999), "xs");
        assert_eq!(input_bucket(2_000), "s");
        assert_eq!(input_bucket(8_000), "m");
        assert_eq!(input_bucket(32_000), "l");
        assert_eq!(input_bucket(128_000), "xl");
    }

    #[test]
    fn model_family_matching() {
        assert_eq!(model_family("claude-opus-4-1"), "opus");
        assert_eq!(model_family("claude-3-5-sonnet"), "sonnet");
        assert_eq!(model_family("gpt-4o"), "gpt");
        assert_eq!(model_family("gemini-2.5-pro"), "gemini");
        assert_eq!(model_family("mystery-model"), "other");
    }

    #[test]
    fn stratum_key_order() {
        let k = stratum_key("continuation", 5000, "claude-sonnet-4", true);
        assert_eq!(k, "sonnet|continuation|s|tools");
    }

    #[test]
    fn accum_mean_var_merge() {
        let mut a = Accum::default();
        for x in [2.0, 4.0, 6.0] {
            a.add(x);
        }
        assert!((a.mean() - 4.0).abs() < 1e-9);
        assert!((a.var() - 4.0).abs() < 1e-9); // sample var of 2,4,6 = 4
        let mut b = Accum::default();
        b.add(8.0);
        a.merge(&b);
        assert_eq!(a.n, 4);
        assert!((a.sum - 20.0).abs() < 1e-9);
    }

    #[test]
    fn accum_var_zero_below_two() {
        let mut a = Accum::default();
        assert_eq!(a.var(), 0.0);
        a.add(5.0);
        assert_eq!(a.var(), 0.0);
    }

    #[test]
    fn baseline_lookup_backoff_and_global() {
        let mut b = BaselineModel::default();
        b.observe("sonnet|cont|s|tools", 100);
        b.observe("sonnet|cont|s|tools", 200);
        // Exact hit.
        let (mean, _v, n) = b.lookup("sonnet|cont|s|tools");
        assert!((mean - 150.0).abs() < 1e-9);
        assert_eq!(n, 2);
        // Back-off: unseen leaf, but prefix "sonnet|cont|s|" matches.
        let (mean, _v, _n) = b.lookup("sonnet|cont|s|notools");
        assert!((mean - 150.0).abs() < 1e-9);
        // Global fallback for a totally unseen family.
        let (mean, _v, _n) = b.lookup("gpt|x|xl|tools");
        assert!((mean - 150.0).abs() < 1e-9); // only global samples exist
    }

    #[test]
    fn assign_arm_extremes_and_stability() {
        assert_eq!(assign_arm("k", 0.0), "treatment");
        assert_eq!(assign_arm("k", 1.0), "control");
        // Deterministic for a fixed key/fraction.
        let a = assign_arm("conv-123", 0.5);
        let b = assign_arm("conv-123", 0.5);
        assert_eq!(a, b);
    }

    #[test]
    fn assign_arm_holdout_distribution() {
        // ~10% land in control over many keys.
        let control = (0..2000)
            .filter(|i| assign_arm(&format!("c{i}"), 0.1) == "control")
            .count();
        assert!((100..=300).contains(&control), "control={control}");
    }

    #[test]
    fn conversation_key_stable_across_turns() {
        let turn1 = json!({"model": "m", "messages": [{"role": "user", "content": "hello"}]});
        let turn2 = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "follow up"}
            ]
        });
        // Same first user message → same conversation key.
        assert_eq!(
            conversation_key_from_body(&turn1),
            conversation_key_from_body(&turn2)
        );
    }

    #[test]
    fn stratum_label_round_trip() {
        let t = stratum_label("treatment", "sonnet|c|s|tools");
        assert_eq!(
            parse_stratum_label(&t),
            Some(("treatment", "sonnet|c|s|tools".to_string()))
        );
        let c = stratum_label("control", "gpt|x|m|notools");
        assert_eq!(
            parse_stratum_label(&c),
            Some(("control", "gpt|x|m|notools".to_string()))
        );
        assert!(parse_stratum_label("router:excluded").is_none());
    }

    #[test]
    fn holdout_estimate_measured() {
        let mut l = SavingsLedger::default();
        // Same stratum in both arms: control ~100, treatment ~70.
        for _ in 0..10 {
            l.record("control", "s|c|s|tools", 100);
            l.record("treatment", "s|c|s|tools", 70);
        }
        let est = l.estimate_from_holdout().unwrap();
        assert_eq!(est.kind, "measured");
        assert!((est.pct - 30.0).abs() < 1e-6);
        assert_eq!(est.n_requests, 10);
    }

    #[test]
    fn holdout_none_without_both_arms() {
        let mut l = SavingsLedger::default();
        l.record("treatment", "k", 50);
        assert!(l.estimate_from_holdout().is_none());
        // best_estimate falls back to baseline path (empty → 0 pct).
        assert_eq!(l.best_estimate().kind, "estimated");
    }

    #[test]
    fn baseline_estimate_signed() {
        let mut l = SavingsLedger::default();
        l.baseline.observe("k", 100);
        l.baseline.observe("k", 100);
        l.record("treatment", "k", 60);
        l.record("treatment", "k", 80);
        let est = l.estimate_from_baseline();
        assert_eq!(est.kind, "estimated");
        // baseline mean 100, treatment mean 70, n=2 → saved 60, baseline 200 → 30%.
        assert!((est.pct - 30.0).abs() < 1e-6);
    }

    #[test]
    fn ledger_save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("os.json");
        let mut l = SavingsLedger::default();
        l.baseline.observe("k", 100);
        l.record("treatment", "k", 70);
        l.save(&path).unwrap();
        let loaded = SavingsLedger::load(&path);
        assert_eq!(loaded.baseline.total_samples(), 1);
        assert_eq!(loaded.treatment.get("k").unwrap().n, 1);
        // Missing file → empty.
        assert_eq!(
            SavingsLedger::load(dir.path().join("nope.json").as_path())
                .baseline
                .total_samples(),
            0
        );
    }

    #[test]
    fn recorder_records_and_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.json");
        let rec = SavingsRecorder::new(path.clone(), 2);
        let label = stratum_label("treatment", "sonnet|c|s|tools");
        assert!(rec.record_from_labels(&[label.clone()], 50));
        // Non-shaping labels ignored.
        assert!(!rec.record_from_labels(&["router:x".to_string()], 10));
        // Second shaping record hits flush_every=2 → file written.
        assert!(rec.record_from_labels(&[label], 60));
        assert!(path.exists());
    }

    #[test]
    fn echo_ratio_overlap() {
        // Output fully contained in context → ratio 1.0 (n=2).
        let out = "the quick brown fox";
        let ctx = "the quick brown fox jumps";
        assert!((echo_ratio(out, ctx, 2) - 1.0).abs() < 1e-9);
        // Output shorter than n → 0.0.
        assert_eq!(echo_ratio("one two", "one two three", 8), 0.0);
        // No overlap.
        assert_eq!(echo_ratio("aa bb cc", "xx yy zz", 2), 0.0);
    }

    // ─── estimate_request_savings (upstream addition) ────────────────────
    //
    // Expected values were produced by running Python's
    // `SavingsRecorder.estimate_request_savings` on the same baseline.

    /// Seed the baseline directly. The baseline is NOT fed by
    /// `record_from_labels` (that fills the control/treatment accumulators);
    /// it comes from the on-disk model `learn --verbosity --apply` writes.
    fn recorder_with_baseline(path: PathBuf, key: &str, samples: &[i64]) -> SavingsRecorder {
        let rec = SavingsRecorder::new(path, 10_000);
        {
            let mut st = rec.state.lock().unwrap();
            for &v in samples {
                st.ledger.baseline.observe(key, v);
            }
        }
        rec
    }

    #[test]
    fn estimate_request_savings_matches_python() {
        let dir = tempfile::tempdir().unwrap();
        let key = "claude|b1|v2";
        let rec = recorder_with_baseline(dir.path().join("e.json"), key, &[90, 100, 110]);
        let treat = stratum_label("treatment", key);
        let ctrl = stratum_label("control", key);

        // Treatment: baseline mean 100 minus what we actually emitted.
        assert_eq!(rec.estimate_request_savings(&[treat.clone()], 60), 40);
        // No saving when we matched or exceeded the baseline — never negative.
        assert_eq!(rec.estimate_request_savings(&[treat], 100), 0);
        // Control and unlabelled requests never claim a saving.
        assert_eq!(rec.estimate_request_savings(&[ctrl], 60), 0);
        assert_eq!(rec.estimate_request_savings(&[], 60), 0);
        // Non-shaping labels are skipped, not treated as a stratum.
        assert_eq!(
            rec.estimate_request_savings(&["router:something".to_string()], 60),
            0
        );
    }

    #[test]
    fn estimate_request_savings_is_read_only() {
        // It must not mutate the ledger, or it would double-count against
        // `record_from_labels`.
        let dir = tempfile::tempdir().unwrap();
        let key = "claude|b1|v2";
        let rec = recorder_with_baseline(dir.path().join("e.json"), key, &[90, 100, 110]);
        let before = {
            let st = rec.state.lock().unwrap();
            serde_json::to_string(&st.ledger).unwrap()
        };
        rec.estimate_request_savings(&[stratum_label("treatment", key)], 60);
        let after = {
            let st = rec.state.lock().unwrap();
            serde_json::to_string(&st.ledger).unwrap()
        };
        assert_eq!(before, after, "estimation must not touch the ledger");
    }

    #[test]
    fn estimate_request_savings_rounds_half_to_even_like_python() {
        // Python's `round()` is banker's rounding: round(0.5) == 0 and
        // round(1.5) == 2. Plain `.round()` would give 1 and 2.
        let dir = tempfile::tempdir().unwrap();
        let key = "m|b|v";
        let rec = recorder_with_baseline(dir.path().join("r.json"), key, &[100, 101]);
        let treat = stratum_label("treatment", key);
        // mean = 100.5
        assert_eq!(rec.estimate_request_savings(&[treat.clone()], 100), 0);
        assert_eq!(rec.estimate_request_savings(&[treat], 99), 2);
    }

    #[test]
    fn unknown_stratum_falls_back_through_the_key_hierarchy() {
        // `lookup` degrades to coarser keys rather than reporting no samples,
        // so an unseen stratum still estimates from its family. Verified
        // against Python, which returns 40 here rather than 0.
        let dir = tempfile::tempdir().unwrap();
        let rec =
            recorder_with_baseline(dir.path().join("f.json"), "claude|b1|v2", &[90, 100, 110]);
        let other = stratum_label("treatment", "zz|b|v");
        assert_eq!(rec.estimate_request_savings(&[other], 60), 40);
    }

    /// A truncating write leaves a half-file behind on a crash, and `load`
    /// reads unparseable JSON as an empty ledger — so a torn write silently
    /// resets the savings history. The rename makes that unrepresentable.
    #[test]
    fn save_leaves_no_temporary_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("ledger.json");

        let ledger = SavingsLedger::default();
        ledger.save(&path).expect("save");

        assert!(path.exists(), "ledger written");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary left behind: {leftovers:?}");

        // Overwriting an existing ledger must also land whole.
        ledger.save(&path).expect("second save");
        let text = std::fs::read_to_string(&path).unwrap();
        serde_json::from_str::<SavingsLedger>(&text).expect("reloads as valid JSON");
    }
}
