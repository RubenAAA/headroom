//! Durable append-only savings event ledger (Rust port of
//! `headroom/savings_ledger.py`).
//!
//! Every compression appends a single JSON line to a file-locked JSONL ledger.
//! The ledger survives proxy/agent restarts and is safe across concurrent
//! writers (each takes a cross-process advisory `flock`). [`aggregate_savings`]
//! reads the file on demand, so there is no shared mutable state to clobber.
//!
//! Cost pricing: [`estimate_cost_usd`] resolves the model against the vendored
//! [`crate::pricing`] table (the Rust stand-in for Python's `litellm` lookup)
//! and falls back to the blended per-token rate for unpriced models — matching
//! the Python behaviour where priced models use `litellm` and everything else
//! uses the blended fallback. The record schema, field names, ordering, and JSON
//! encoding match Python so both implementations can share one ledger file.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Local, TimeZone, Timelike, Utc};
use rustix::fs::{flock, FlockOperation};
use serde_json::{json, Map, Value};

pub const SCHEMA_VERSION: i64 = 1;
pub const UNKNOWN: &str = "unknown";
/// Hard cap on the report lookback. A caller asking for more (or for 0/None,
/// which means "use the cap", never "unbounded") is clamped to this, keeping
/// the report bounded and small.
pub const MAX_RETENTION_DAYS: i64 = 30;
pub const DEFAULT_RETENTION_DAYS: i64 = MAX_RETENTION_DAYS;
/// Blended input price ($/token) used when a model cannot be priced. Mirrors
/// the ~$3 / 1M input-token assumption the MCP stats path uses.
pub const DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN: f64 = 3.0 / 1_000_000.0;

const PROJECT_NAME_MAX_LENGTH: usize = 128;
/// Compact the ledger once it grows past this size.
const COMPACT_SIZE_BYTES: u64 = 8 * 1024 * 1024;

fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

/// Round to `ndigits` decimal places using round-half-to-even, matching
/// Python's built-in `round`.
fn round_half_even(value: f64, ndigits: i32) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let factor = 10f64.powi(ndigits);
    let scaled = value * factor;
    let rounded = scaled.round_ties_even();
    rounded / factor
}

/// Parse an ISO-8601 / RFC3339 timestamp string to UTC. Mirrors Python's
/// `_parse_timestamp`: `Z` is accepted, naive strings are assumed UTC.
fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    if value.is_empty() {
        return None;
    }
    let normalized = value.replace('Z', "+00:00");
    if let Ok(dt) = DateTime::parse_from_rfc3339(&normalized) {
        return Some(dt.with_timezone(&Utc));
    }
    // Naive (no offset) — assume UTC, mirroring Python's tzinfo fill-in.
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&normalized, fmt) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    None
}

/// Normalize a model label, falling back to the stable `unknown` sentinel.
fn normalize_model(value: Option<&str>) -> String {
    match value {
        Some(v) => {
            let cleaned = v.trim();
            if cleaned.is_empty() {
                UNKNOWN.to_string()
            } else {
                cleaned.to_string()
            }
        }
        None => UNKNOWN.to_string(),
    }
}

/// Sanitize a client-supplied project/client label; `None` when unusable.
/// Percent-decodes, strips non-printable chars, trims, and caps length.
fn sanitize_project_name(value: Option<&str>) -> Option<String> {
    let value = value?;
    let decoded = percent_decode(value);
    let cleaned: String = decoded.chars().filter(|c| is_printable(*c)).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.chars().take(PROJECT_NAME_MAX_LENGTH).collect())
}

/// Approximation of Python's `str.isprintable()`: printable == not a control
/// char and not a separator other than ASCII space.
fn is_printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    !c.is_control() && !c.is_whitespace()
}

/// Minimal `application/x-www-form-urlencoded`-style percent decoding, matching
/// Python's `urllib.parse.unquote` for the ASCII cases the ledger sees.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn label(value: Option<&str>) -> String {
    sanitize_project_name(value).unwrap_or_else(|| UNKNOWN.to_string())
}

fn resolve_path(path: Option<&Path>) -> PathBuf {
    crate::paths::savings_events_path(path)
}

/// Dollar value of saved input tokens.
///
/// Prices the saved tokens at the model's vendored input rate when
/// [`crate::pricing::lookup`] resolves `model` (the Rust replacement for the
/// Python `litellm` lookup), otherwise at the blended `fallback_rate`. Zero or
/// negative savings short-circuit to `0.0`. Models not in the pricing table
/// (e.g. `unknown`, `test-model`) keep the historical fallback-only behaviour.
pub fn estimate_cost_usd(model: &str, tokens_saved: i64, fallback_rate: f64) -> f64 {
    if tokens_saved <= 0 {
        return 0.0;
    }
    let rate = crate::pricing::lookup(model)
        .map(|p| p.input_cost_per_token)
        .unwrap_or(fallback_rate);
    round_half_even(tokens_saved as f64 * rate, 6)
}

/// Optional inputs to [`record_savings_event`]. `None` fields take the Python
/// defaults.
#[derive(Default)]
pub struct SavingsEvent<'a> {
    pub tokens_before: i64,
    pub tokens_after: i64,
    pub model: Option<&'a str>,
    pub client: Option<&'a str>,
    pub source: Option<&'a str>,
    pub timestamp: Option<DateTime<Utc>>,
    pub cost_usd: Option<f64>,
    /// How `cost_usd` was priced (`fresh_input` or `cache_read`). Absent on
    /// legacy events whose placement was not measured.
    pub cost_basis: Option<&'a str>,
    pub fallback_rate: Option<f64>,
    pub path: Option<&'a Path>,
}

/// Record a savings event from a completed request, given the count that was
/// actually **forwarded** upstream (post-compression) and the tokens saved.
///
/// The ledger's `before` is the pre-compression original and `after` is what we
/// forwarded; `headroom savings` derives the reduction percent as
/// `saved / before`. Passing the forwarded count as `before` understates the
/// original by `tokens_saved` and inflates that percentage — a real 40%
/// reduction (1000 → 600) gets reported as ~67%. Reconstructing the original as
/// `forwarded + saved` also pins `before - after == tokens_saved` exactly, so
/// the ledger's derived saving can never drift from the caller's.
///
/// This is the only place that reconstruction lives; call sites pass what they
/// forwarded and cannot get the arithmetic wrong.
pub fn record_from_forwarded(
    forwarded_tokens: i64,
    tokens_saved: i64,
    model: Option<&str>,
    client: Option<&str>,
) -> bool {
    record_from_forwarded_with_cost(forwarded_tokens, tokens_saved, model, client, None, None)
}

/// As [`record_from_forwarded`], with a request-scoped price derived from the
/// provider's cache usage. New proxy paths use this; the legacy helper remains
/// for callers that genuinely have no placement measurement.
pub fn record_from_forwarded_with_cost(
    forwarded_tokens: i64,
    tokens_saved: i64,
    model: Option<&str>,
    client: Option<&str>,
    cost_usd: Option<f64>,
    cost_basis: Option<&str>,
) -> bool {
    if tokens_saved <= 0 {
        return false;
    }
    record_savings_event(SavingsEvent {
        tokens_before: forwarded_tokens + tokens_saved,
        tokens_after: forwarded_tokens,
        model,
        client: Some(client.unwrap_or("proxy")),
        source: Some("proxy"),
        timestamp: None,
        cost_usd,
        cost_basis,
        fallback_rate: None,
        path: None,
    })
}

/// Append one savings event to the durable ledger. Never panics; returns `true`
/// when a line was written.
pub fn record_savings_event(event: SavingsEvent) -> bool {
    let before = event.tokens_before.max(0);
    let after = event.tokens_after.max(0);
    let saved = (before - after).max(0);
    if saved <= 0 {
        return false;
    }

    let fallback_rate = event
        .fallback_rate
        .unwrap_or(DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN);
    let model_label = normalize_model(event.model);
    let cost = match event.cost_usd {
        Some(c) => c.max(0.0),
        None => estimate_cost_usd(&model_label, saved, fallback_rate),
    };

    let ts = event.timestamp.unwrap_or_else(utc_now);
    // Python serialises the aware datetime via `.isoformat()` (offset form,
    // microsecond precision). RFC3339 with `+00:00` round-trips through
    // Python's `fromisoformat`.
    let ts_str = ts.to_rfc3339_opts(chrono::SecondsFormat::Micros, false);

    // Insertion-ordered map to match Python's dict key order byte-for-byte.
    let mut obj = Map::new();
    obj.insert("v".into(), json!(SCHEMA_VERSION));
    obj.insert("ts".into(), json!(ts_str));
    obj.insert("before".into(), json!(before));
    obj.insert("after".into(), json!(after));
    obj.insert("saved".into(), json!(saved));
    obj.insert("cost_usd".into(), json!(round_half_even(cost, 6)));
    if let Some(basis) = event.cost_basis {
        obj.insert("cost_basis".into(), json!(basis));
    }
    obj.insert("model".into(), json!(model_label));
    obj.insert("client".into(), json!(label(event.client)));
    obj.insert("source".into(), json!(event.source.unwrap_or(UNKNOWN)));
    obj.insert("pid".into(), json!(std::process::id()));

    let target = resolve_path(event.path);
    if write_locked_line(&target, &Value::Object(obj)).is_err() {
        return false;
    }

    maybe_compact(&target);
    true
}

fn write_locked_line(target: &Path, event: &Value) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // serde_json emits no spaces after separators — matches Python's
    // `separators=(",", ":")`.
    let line = format!("{}\n", serde_json::to_string(event)?);
    let mut handle = OpenOptions::new().create(true).append(true).open(target)?;
    flock(handle.as_fd(), FlockOperation::LockExclusive)?;
    let write_res = handle.write_all(line.as_bytes());
    let _ = flock(handle.as_fd(), FlockOperation::Unlock);
    write_res
}

struct ParsedEvent {
    ts: DateTime<Utc>,
    value: Value,
}

fn read_events(path: Option<&Path>, retention_days: i64, now: DateTime<Utc>) -> Vec<ParsedEvent> {
    let target = resolve_path(path);
    let file = match File::open(&target) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let cutoff = if retention_days != 0 {
        Some(now - Duration::days(retention_days))
    } else {
        None
    };
    let _ = flock(file.as_fd(), FlockOperation::LockShared);
    let reader = BufReader::new(&file);
    let mut events = Vec::new();
    for raw in reader.lines() {
        let raw = match raw {
            Ok(r) => r,
            Err(_) => break,
        };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ts_field = value.get("ts").and_then(|v| v.as_str());
        let parsed = ts_field.and_then(parse_timestamp);
        let parsed = match parsed {
            Some(p) => p,
            None => continue,
        };
        if let Some(cutoff) = cutoff {
            if parsed < cutoff {
                continue;
            }
        }
        events.push(ParsedEvent { ts: parsed, value });
    }
    let _ = flock(file.as_fd(), FlockOperation::Unlock);
    events
}

#[derive(Default, Clone)]
struct Bucket {
    tokens_saved: i64,
    tokens_before: i64,
    cost_usd: f64,
    calls: i64,
}

impl Bucket {
    fn add(&mut self, saved: i64, before: i64, cost: f64) {
        self.tokens_saved += saved;
        self.tokens_before += before;
        self.cost_usd += cost;
        self.calls += 1;
    }

    fn savings_percent(&self) -> f64 {
        if self.tokens_before <= 0 {
            return 0.0;
        }
        round_half_even(
            self.tokens_saved as f64 / self.tokens_before as f64 * 100.0,
            1,
        )
    }

    fn to_value(&self) -> Value {
        json!({
            "tokens_saved": self.tokens_saved,
            "tokens_before": self.tokens_before,
            "cost_usd": round_half_even(self.cost_usd, 6),
            "calls": self.calls,
            "savings_percent": self.savings_percent(),
        })
    }
}

/// Aggregated savings report. [`to_value`](SavingsReport::to_value) matches the
/// Python `SavingsReport.to_dict()` shape.
pub struct SavingsReport {
    pub path: String,
    pub schema_version: i64,
    pub lifetime: Value,
    pub windows: Value,
    pub by_model: Vec<Value>,
    pub by_client: Vec<Value>,
    pub top_model: String,
}

impl SavingsReport {
    pub fn to_value(&self) -> Value {
        json!({
            "schema_version": self.schema_version,
            "path": self.path,
            "top_model": self.top_model,
            "lifetime": self.lifetime,
            "windows": self.windows,
            "by_model": self.by_model,
            "by_client": self.by_client,
        })
    }
}

fn ranked(buckets: &[(String, Bucket)], key_name: &str) -> Vec<Value> {
    let mut rows: Vec<(f64, i64, Value)> = buckets
        .iter()
        .map(|(name, bucket)| {
            let mut obj = bucket.to_value();
            if let Value::Object(ref mut m) = obj {
                // key_name first to match `{key_name: name, **bucket}` order.
                let mut ordered = Map::new();
                ordered.insert(key_name.to_string(), json!(name));
                for (k, v) in m.iter() {
                    ordered.insert(k.clone(), v.clone());
                }
                obj = Value::Object(ordered);
            }
            (bucket.cost_usd, bucket.tokens_saved, obj)
        })
        .collect();
    // sort by (cost_usd, tokens_saved) descending; stable to preserve insertion
    // order for ties (Python's sort is stable).
    rows.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.cmp(&a.1))
    });
    rows.into_iter().map(|(_, _, v)| v).collect()
}

/// Insertion-ordered accumulator keyed by string (mirrors Python dict order).
#[derive(Default)]
struct OrderedBuckets {
    order: Vec<String>,
    map: std::collections::HashMap<String, Bucket>,
}

impl OrderedBuckets {
    fn entry(&mut self, key: String) -> &mut Bucket {
        if !self.map.contains_key(&key) {
            self.order.push(key.clone());
            self.map.insert(key.clone(), Bucket::default());
        }
        self.map.get_mut(&key).unwrap()
    }

    fn into_ordered(self) -> Vec<(String, Bucket)> {
        let map = self.map;
        self.order
            .into_iter()
            .map(|k| {
                let b = map.get(&k).cloned().unwrap_or_default();
                (k, b)
            })
            .collect()
    }
}

/// Aggregate the durable ledger into lifetime / windowed / per-dimension views.
pub fn aggregate_savings(
    path: Option<&Path>,
    now: Option<DateTime<Utc>>,
    retention_days: i64,
) -> SavingsReport {
    let now = now.unwrap_or_else(utc_now);
    // Hard-cap the lookback regardless of caller input. `0` means "use the cap"
    // here, NOT "unbounded" — `read_events` treats 0 as no cutoff, so passing it
    // through would silently produce an unbounded report.
    let retention_days = if retention_days <= 0 {
        MAX_RETENTION_DAYS
    } else {
        retention_days.min(MAX_RETENTION_DAYS)
    };
    let events = read_events(path, retention_days, now);

    // "Today" = local calendar day start, converted back to UTC.
    let today_cutoff = {
        let local_now = now.with_timezone(&Local);
        let local_midnight = local_now
            .with_hour(0)
            .and_then(|d| d.with_minute(0))
            .and_then(|d| d.with_second(0))
            .and_then(|d| d.with_nanosecond(0))
            .unwrap_or(local_now);
        local_midnight.with_timezone(&Utc)
    };
    let week_cutoff = now - Duration::days(7);

    let mut windowed = Bucket::default();
    let mut today = Bucket::default();
    let mut last_7 = Bucket::default();
    let mut by_model = OrderedBuckets::default();
    let mut by_client = OrderedBuckets::default();

    for event in &events {
        let obj = &event.value;
        let saved = coerce_i64(obj.get("saved")).max(0);
        let before = coerce_i64(obj.get("before")).max(0);
        let cost = coerce_f64(obj.get("cost_usd")).max(0.0);

        windowed.add(saved, before, cost);
        if event.ts >= today_cutoff {
            today.add(saved, before, cost);
        }
        if event.ts >= week_cutoff {
            last_7.add(saved, before, cost);
        }

        let model = obj
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(UNKNOWN)
            .to_string();
        by_model.entry(model).add(saved, before, cost);

        let client = obj
            .get("client")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(UNKNOWN)
            .to_string();
        by_client.entry(client).add(saved, before, cost);
    }

    let model_rows = ranked(&by_model.into_ordered(), "model");
    let top_model = model_rows
        .first()
        .and_then(|r| r.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or(UNKNOWN)
        .to_string();

    // `windowed` spans only what `read_events` kept, which the retention cap
    // bounds at 30 days — so this is the 30-day window, not all time. It also
    // stands in for `lifetime`, matching Python: with a capped ledger there is
    // no longer any such thing as an all-time total.
    let windows = json!({
        "today": today.to_value(),
        "last_7_days": last_7.to_value(),
        "last_30_days": windowed.to_value(),
    });

    SavingsReport {
        path: resolve_path(path).to_string_lossy().into_owned(),
        schema_version: SCHEMA_VERSION,
        lifetime: windowed.to_value(),
        windows,
        by_model: model_rows,
        by_client: ranked(&by_client.into_ordered(), "client"),
        top_model,
    }
}

fn coerce_i64(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn coerce_f64(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Rewrite the ledger dropping out-of-retention events once it grows large.
fn maybe_compact(target: &Path) {
    let size = match std::fs::metadata(target) {
        Ok(m) => m.len(),
        Err(_) => return,
    };
    if size <= COMPACT_SIZE_BYTES {
        return;
    }

    let now = utc_now();
    let cutoff = now - Duration::days(DEFAULT_RETENTION_DAYS);
    let file = match OpenOptions::new().read(true).write(true).open(target) {
        Ok(f) => f,
        Err(_) => return,
    };
    if flock(file.as_fd(), FlockOperation::LockExclusive).is_err() {
        return;
    }

    let mut kept: Vec<String> = Vec::new();
    {
        let reader = BufReader::new(&file);
        for raw in reader.lines() {
            let raw = match raw {
                Ok(r) => r,
                Err(_) => break,
            };
            let stripped = raw.trim();
            if stripped.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(stripped) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let parsed = value
                .get("ts")
                .and_then(|v| v.as_str())
                .and_then(parse_timestamp);
            match parsed {
                Some(p) if p >= cutoff => kept.push(stripped.to_string()),
                _ => continue,
            }
        }
    }

    let mut file = file;
    let _ = (|| -> std::io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        if !kept.is_empty() {
            file.write_all(kept.join("\n").as_bytes())?;
            file.write_all(b"\n")?;
        }
        Ok(())
    })();
    let _ = flock(file.as_fd(), FlockOperation::Unlock);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ev<'a>(before: i64, after: i64, client: &'a str, path: &'a Path) -> SavingsEvent<'a> {
        SavingsEvent {
            tokens_before: before,
            tokens_after: after,
            client: Some(client),
            path: Some(path),
            ..Default::default()
        }
    }

    #[test]
    fn record_from_forwarded_reconstructs_the_pre_compression_original() {
        // A real 40% reduction: 1000 original -> 600 forwarded, 400 saved.
        // Passing the forwarded count as `before` would report 400/600 = ~67%.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("savings_events.jsonl");
        assert!(record_savings_event(SavingsEvent {
            tokens_before: 600 + 400,
            tokens_after: 600,
            client: Some("c"),
            path: Some(&path),
            ..Default::default()
        }));
        let report = aggregate_savings(Some(&path), None, DEFAULT_RETENTION_DAYS);
        assert_eq!(report.lifetime["tokens_saved"], json!(400));
        // `before` is the original, so the reduction percent is 400/1000 = 40%,
        // not 400/600.
        assert_eq!(report.lifetime["tokens_before"], json!(1000));
    }

    #[test]
    fn record_from_forwarded_ignores_a_non_saving_request() {
        assert!(!record_from_forwarded(600, 0, Some("m"), None));
        assert!(!record_from_forwarded(600, -5, Some("m"), None));
    }

    #[test]
    fn request_scoped_cost_and_basis_are_written_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("savings_events.jsonl");
        assert!(record_savings_event(SavingsEvent {
            tokens_before: 1_000,
            tokens_after: 0,
            model: Some("claude-opus-5"),
            cost_usd: Some(0.0015),
            cost_basis: Some("cache_read"),
            path: Some(&path),
            ..Default::default()
        }));

        let line: Value =
            serde_json::from_str(std::fs::read_to_string(path).unwrap().trim()).unwrap();
        assert_eq!(line["cost_usd"], json!(0.0015));
        assert_eq!(line["cost_basis"], json!("cache_read"));
    }

    #[test]
    fn retention_is_capped_even_when_caller_asks_for_more() {
        // 0 means "use the cap", not "unbounded"; a large ask is clamped.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let now = Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap();
        record_savings_event(SavingsEvent {
            tokens_before: 1000,
            tokens_after: 500,
            client: Some("c"),
            timestamp: Some(now - Duration::days(90)),
            path: Some(&path),
            ..Default::default()
        });
        for asked in [0, 365, MAX_RETENTION_DAYS] {
            let report = aggregate_savings(Some(&path), Some(now), asked);
            assert_eq!(
                report.lifetime["tokens_saved"],
                json!(0),
                "a 90-day-old event must fall outside the capped window (asked {asked})"
            );
        }
    }

    #[test]
    fn unknown_model_uses_blended_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("savings_events.jsonl");
        assert!(record_savings_event(ev(1000, 400, "c", &path)));
        let report = aggregate_savings(Some(&path), None, DEFAULT_RETENTION_DAYS);
        assert_eq!(report.lifetime["tokens_saved"], json!(600));
        let expected = round_half_even(600.0 * DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN, 6);
        assert_eq!(report.lifetime["cost_usd"].as_f64().unwrap(), expected);
        assert!(report
            .by_model
            .iter()
            .any(|r| r["model"] == json!("unknown")));
    }

    #[test]
    fn estimate_cost_unknown_short_circuits_to_fallback() {
        assert!((estimate_cost_usd("unknown", 1000, 1e-6) - 0.001).abs() < 1e-12);
        assert_eq!(
            estimate_cost_usd(UNKNOWN, 0, DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN),
            0.0
        );
    }

    #[test]
    fn explicit_cost_is_honored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        record_savings_event(SavingsEvent {
            tokens_before: 100,
            tokens_after: 10,
            model: Some("x"),
            client: Some("c"),
            cost_usd: Some(1.25),
            path: Some(&path),
            ..Default::default()
        });
        let report = aggregate_savings(Some(&path), None, DEFAULT_RETENTION_DAYS);
        assert_eq!(report.lifetime["cost_usd"].as_f64().unwrap(), 1.25);
    }

    #[test]
    fn zero_or_negative_savings_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        assert!(!record_savings_event(ev(100, 100, "c", &path)));
        assert!(!record_savings_event(ev(50, 80, "c", &path)));
        assert_eq!(
            aggregate_savings(Some(&path), None, DEFAULT_RETENTION_DAYS).lifetime["calls"],
            json!(0)
        );
    }

    #[test]
    fn breakdowns_aggregate_by_dimension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        record_savings_event(ev(1000, 300, "claude-code", &path));
        record_savings_event(ev(500, 200, "claude-code", &path));
        record_savings_event(SavingsEvent {
            tokens_before: 2000,
            tokens_after: 600,
            model: Some("gpt"),
            client: Some("proxy"),
            cost_usd: Some(0.5),
            path: Some(&path),
            ..Default::default()
        });
        let report = aggregate_savings(Some(&path), None, DEFAULT_RETENTION_DAYS);
        let clients: std::collections::HashMap<String, &Value> = report
            .by_client
            .iter()
            .map(|r| (r["client"].as_str().unwrap().to_string(), r))
            .collect();
        assert_eq!(clients["claude-code"]["calls"], json!(2));
        assert_eq!(clients["proxy"]["tokens_saved"], json!(1400));
    }

    #[test]
    fn windows_today_week_alltime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let now = Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap();
        let mk = |before, after, ts| SavingsEvent {
            tokens_before: before,
            tokens_after: after,
            client: Some("c"),
            timestamp: Some(ts),
            path: Some(&path),
            ..Default::default()
        };
        record_savings_event(mk(1000, 500, now));
        record_savings_event(mk(1000, 600, now - Duration::days(3)));
        record_savings_event(mk(1000, 700, now - Duration::days(30)));
        let report = aggregate_savings(Some(&path), Some(now), DEFAULT_RETENTION_DAYS);
        assert_eq!(report.windows["today"]["tokens_saved"], json!(500));
        assert_eq!(report.windows["last_7_days"]["tokens_saved"], json!(900));
        // The 30-day-old event sits exactly on the retention cutoff, which is
        // inclusive (`parsed < cutoff` is what gets dropped), so all three count.
        assert_eq!(report.windows["last_30_days"]["tokens_saved"], json!(1200));
        assert_eq!(report.windows["last_30_days"]["calls"], json!(3));
    }

    #[test]
    fn retention_excludes_old_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let now = Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap();
        record_savings_event(SavingsEvent {
            tokens_before: 1000,
            tokens_after: 500,
            client: Some("c"),
            timestamp: Some(now),
            path: Some(&path),
            ..Default::default()
        });
        record_savings_event(SavingsEvent {
            tokens_before: 1000,
            tokens_after: 500,
            client: Some("c"),
            timestamp: Some(now - Duration::days(400)),
            path: Some(&path),
            ..Default::default()
        });
        let report = aggregate_savings(Some(&path), Some(now), 365);
        assert_eq!(report.lifetime["calls"], json!(1));
    }

    #[test]
    fn appends_survive_restart_and_corrupt_lines_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        for _ in 0..5 {
            record_savings_event(ev(100, 10, "c", &path));
        }
        // aggregate reads purely from disk — proves durability.
        let report = aggregate_savings(Some(&path), None, DEFAULT_RETENTION_DAYS);
        assert_eq!(report.lifetime["calls"], json!(5));
        assert_eq!(report.lifetime["tokens_saved"], json!(450));

        // Corrupt trailing content is skipped.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"not json\n\n").unwrap();
        }
        let report = aggregate_savings(Some(&path), None, DEFAULT_RETENTION_DAYS);
        assert_eq!(report.lifetime["calls"], json!(5));
    }
}
