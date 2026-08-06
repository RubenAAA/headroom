//! Analyze Headroom proxy logs for performance insights (Rust port of
//! `headroom/perf/analyzer.py`).
//!
//! Parses PERF log lines from `~/.headroom/logs/proxy.log*` and produces
//! reports on token savings, cache efficiency, and transform impact.
//!
//! Deviations from Python: list prices come from the vendored
//! [`crate::pricing`] table instead of LiteLLM; the TOIN-highlights section
//! (which reads the Python-only live pattern store) is omitted; and the
//! context-tool lifetime section supports RTK only (not lean-ctx). Python
//! already renders the report without these sections when the backends are
//! unavailable, so the degraded shape is identical.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use chrono::{Duration as ChronoDuration, Local, NaiveDateTime};
use regex::Regex;
use serde::Serialize;

use crate::paths;

fn perf_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?P<ts>\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2},\d+) .* \[(?P<rid>[^\]]+)\] PERF (?P<kv>.+)$",
        )
        .unwrap()
    })
}

fn router_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"content_router: (?P<msgs>\d+) msgs — (?P<detail>.+)$").unwrap())
}

fn transform_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"Transform (?P<name>\w+): (?P<before>\d+) -> (?P<after>\d+) tokens \(saved (?P<saved>\d+)\)",
        )
        .unwrap()
    })
}

fn toin_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"TOIN: (?P<patterns>\d+) patterns, (?P<compressions>\d+) compressions, (?P<retrievals>\d+) retrievals, (?P<rate>[\d.]+)% retrieval rate",
        )
        .unwrap()
    })
}

fn stage_timings_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?P<ts>\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2},\d+) .* \[(?P<rid>[^\]]+)\] STAGE_TIMINGS (?P<payload>.+)$",
        )
        .unwrap()
    })
}

/// List input price per 1M tokens from the vendored table (LiteLLM in Python).
/// Mirrors Python truthiness: a zero price reads as "unknown".
fn get_list_price(model: &str) -> Option<f64> {
    crate::pricing::lookup(model)
        .map(|p| p.input_cost_per_token * 1_000_000.0)
        .filter(|v| *v > 0.0)
}

/// Parse key=value pairs from a PERF log line. The `transforms=` field is
/// always last and its value may contain spaces (e.g.
/// `transforms=router:excluded:tool*32 read_lifecycle:stale*17`).
fn parse_kv(kv_str: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let mut head = kv_str;
    if let Some(idx) = kv_str.find("transforms=") {
        let (before, rest) = kv_str.split_at(idx);
        let transforms_val = &rest["transforms=".len()..];
        let mut transform_parts: Vec<&str> = Vec::new();
        for part in transforms_val.split_whitespace() {
            if let Some((k, v)) = part.split_once('=') {
                result.insert(k.to_string(), v.to_string());
            } else {
                transform_parts.push(part);
            }
        }
        result.insert("transforms".to_string(), transform_parts.join(" "));
        head = before;
    }
    for part in head.split_whitespace() {
        if let Some((k, v)) = part.split_once('=') {
            result.insert(k.to_string(), v.to_string());
        }
    }
    result
}

/// A single parsed PERF log entry.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PerfRecord {
    pub timestamp: String,
    pub request_id: String,
    pub model: String,
    pub client: String,
    pub num_messages: i64,
    pub tokens_before: i64,
    pub tokens_after: i64,
    pub tokens_saved: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cache_hit_pct: i64,
    pub optimization_ms: f64,
    pub transforms: Vec<String>,
    pub total_ms: f64,
    pub tokens_out: i64,
    pub ttfb_ms: f64,
    pub stages: HashMap<String, f64>,
}

/// A parsed content_router summary line.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RouterRecord {
    pub timestamp: String,
    pub num_messages: i64,
    pub compressed: i64,
    pub excluded: i64,
    pub skipped: i64,
    pub unchanged: i64,
    pub content_blocks: i64,
}

/// A parsed per-transform line.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TransformRecord {
    pub timestamp: String,
    pub name: String,
    pub tokens_before: i64,
    pub tokens_after: i64,
    pub tokens_saved: i64,
}

/// A parsed TOIN status line.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ToinRecord {
    pub timestamp: String,
    pub patterns: i64,
    pub compressions: i64,
    pub retrievals: i64,
    pub retrieval_rate: f64,
}

/// Aggregated performance report.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PerfReport {
    pub perf_records: Vec<PerfRecord>,
    pub router_records: Vec<RouterRecord>,
    pub transform_records: Vec<TransformRecord>,
    pub toin_records: Vec<ToinRecord>,
    pub log_files_read: i64,
    pub total_lines_parsed: i64,
    pub requested_hours: Option<f64>,
    pub oldest_kept_ts: Option<String>,
    pub newest_kept_ts: Option<String>,
    pub records_filtered_out: i64,
    /// True when no time cutoff was applied (`--hours 0` or overflow).
    pub window_all_data: bool,
}

// Log timestamps come from Python's logging formatter: `YYYY-MM-DD HH:MM:SS,fff`.
// Millisecond precision is irrelevant for hour-scale windows, so we parse the
// whole-second prefix and stay permissive: unparsable records are kept.
fn parse_log_ts(ts: &str) -> Option<NaiveDateTime> {
    let seconds = ts.split(',').next()?;
    NaiveDateTime::parse_from_str(seconds, "%Y-%m-%d %H:%M:%S").ok()
}

/// Streaming line parser so tests can feed lines without touching the fs.
struct Parser {
    report: PerfReport,
    stages_by_rid: HashMap<String, HashMap<String, f64>>,
    cutoff: Option<NaiveDateTime>,
}

impl Parser {
    fn new(last_n_hours: f64) -> Self {
        // "Look back a billion hours" is effectively "all data"; treat
        // overflow as no cutoff (mirrors Python's OverflowError handling).
        let cutoff = if last_n_hours > 0.0 {
            let seconds = last_n_hours * 3600.0;
            if seconds.is_finite() && seconds < i64::MAX as f64 {
                Local::now()
                    .naive_local()
                    .checked_sub_signed(ChronoDuration::seconds(seconds as i64))
            } else {
                None
            }
        } else {
            None
        };
        let report = PerfReport {
            requested_hours: Some(last_n_hours),
            window_all_data: cutoff.is_none(),
            ..Default::default()
        };
        Self {
            report,
            stages_by_rid: HashMap::new(),
            cutoff,
        }
    }

    /// Fail-open: records without a parseable timestamp are kept.
    fn within_window(&self, ts: &str) -> bool {
        let Some(cutoff) = self.cutoff else {
            return true;
        };
        match parse_log_ts(ts) {
            Some(parsed) => parsed >= cutoff,
            None => true,
        }
    }

    fn track_window(&mut self, ts: &str) {
        if ts.is_empty() {
            return;
        }
        if self
            .report
            .oldest_kept_ts
            .as_deref()
            .map(|old| ts < old)
            .unwrap_or(true)
        {
            self.report.oldest_kept_ts = Some(ts.to_string());
        }
        if self
            .report
            .newest_kept_ts
            .as_deref()
            .map(|new| ts > new)
            .unwrap_or(true)
        {
            self.report.newest_kept_ts = Some(ts.to_string());
        }
    }

    fn feed_line(&mut self, raw_line: &str) {
        self.report.total_lines_parsed += 1;
        let line = raw_line.trim_end();

        if let Some(m) = stage_timings_re().captures(line) {
            let ts = &m["ts"];
            if !self.within_window(ts) {
                self.report.records_filtered_out += 1;
                return;
            }
            let ts = ts.to_string();
            self.track_window(&ts);
            if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&m["payload"]) {
                if let Some(stages) = payload.get("stages").and_then(|v| v.as_object()) {
                    let parsed: HashMap<String, f64> = stages
                        .iter()
                        .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                        .collect();
                    self.stages_by_rid.insert(m["rid"].to_string(), parsed);
                }
            }
            return;
        }

        if let Some(m) = perf_re().captures(line) {
            let kv = parse_kv(&m["kv"]);
            let transforms_str = kv.get("transforms").map(String::as_str).unwrap_or("none");
            let transforms: Vec<String> = if transforms_str == "none" {
                Vec::new()
            } else if transforms_str.contains('*') || transforms_str.contains(' ') {
                // New format: "router:excluded:tool*32 read_lifecycle:stale*17"
                transforms_str
                    .split_whitespace()
                    .map(|part| match part.rsplit_once('*') {
                        Some((name, _)) => name.to_string(),
                        None => part.to_string(),
                    })
                    .collect()
            } else {
                // Old comma-separated format.
                transforms_str.split(',').map(str::to_string).collect()
            };
            let ts = &m["ts"];
            if !self.within_window(ts) {
                self.report.records_filtered_out += 1;
                return;
            }
            let ts = ts.to_string();
            self.track_window(&ts);
            let int = |key: &str| -> i64 { kv.get(key).and_then(|v| v.parse().ok()).unwrap_or(0) };
            let float =
                |key: &str| -> f64 { kv.get(key).and_then(|v| v.parse().ok()).unwrap_or(0.0) };
            let rid = m["rid"].to_string();
            self.report.perf_records.push(PerfRecord {
                timestamp: ts,
                model: kv.get("model").cloned().unwrap_or_default(),
                client: kv.get("client").cloned().unwrap_or_default(),
                num_messages: int("msgs"),
                tokens_before: int("tok_before"),
                tokens_after: int("tok_after"),
                tokens_saved: int("tok_saved"),
                cache_read: int("cache_read"),
                cache_write: int("cache_write"),
                cache_hit_pct: int("cache_hit_pct"),
                optimization_ms: float("opt_ms"),
                transforms,
                total_ms: float("total_ms"),
                tokens_out: int("tok_out"),
                ttfb_ms: float("ttfb_ms"),
                stages: self.stages_by_rid.get(&rid).cloned().unwrap_or_default(),
                request_id: rid,
            });
            return;
        }

        if line.contains("content_router:") && line.contains("msgs") {
            if let Some(m) = router_re().captures(line) {
                let ts: String = line.chars().take(23).collect();
                if !self.within_window(&ts) {
                    self.report.records_filtered_out += 1;
                    return;
                }
                self.track_window(&ts);
                let mut rec = RouterRecord {
                    timestamp: ts,
                    num_messages: m["msgs"].parse().unwrap_or(0),
                    ..Default::default()
                };
                static NUM_KIND: OnceLock<Regex> = OnceLock::new();
                let num_kind = NUM_KIND.get_or_init(|| Regex::new(r"^(\d+)\s+(\w+)").unwrap());
                for part in m["detail"].split(',') {
                    let part = part.trim();
                    if let Some(nm) = num_kind.captures(part) {
                        let count: i64 = nm[1].parse().unwrap_or(0);
                        match &nm[2] {
                            "compressed" => rec.compressed = count,
                            "excluded" => rec.excluded = count,
                            "skipped" => rec.skipped = count,
                            "unchanged" => rec.unchanged = count,
                            "content" if part.contains("block") => rec.content_blocks = count,
                            _ => {}
                        }
                    }
                }
                self.report.router_records.push(rec);
                return;
            }
        }

        if let Some(m) = transform_re().captures(line) {
            let ts: String = line.chars().take(23).collect();
            if !self.within_window(&ts) {
                self.report.records_filtered_out += 1;
                return;
            }
            self.track_window(&ts);
            self.report.transform_records.push(TransformRecord {
                timestamp: ts,
                name: m["name"].to_string(),
                tokens_before: m["before"].parse().unwrap_or(0),
                tokens_after: m["after"].parse().unwrap_or(0),
                tokens_saved: m["saved"].parse().unwrap_or(0),
            });
            return;
        }

        if let Some(m) = toin_re().captures(line) {
            let ts: String = line.chars().take(23).collect();
            if !self.within_window(&ts) {
                self.report.records_filtered_out += 1;
                return;
            }
            self.track_window(&ts);
            self.report.toin_records.push(ToinRecord {
                timestamp: ts,
                patterns: m["patterns"].parse().unwrap_or(0),
                compressions: m["compressions"].parse().unwrap_or(0),
                retrievals: m["retrievals"].parse().unwrap_or(0),
                retrieval_rate: m["rate"].parse().unwrap_or(0.0),
            });
        }
    }
}

/// Directory the proxy writes its logs to.
pub fn log_dir() -> PathBuf {
    paths::workspace_dir().join("logs")
}

/// Parse all proxy log files (`proxy.log*`, oldest mtime first) and return
/// structured records. `last_n_hours == 0` means all data.
pub fn parse_log_files(last_n_hours: f64) -> PerfReport {
    let mut parser = Parser::new(last_n_hours);

    let dir = log_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return parser.report;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("proxy.log"))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    files.sort();

    for (_, path) in files {
        parser.report.log_files_read += 1;
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            parser.feed_line(line);
        }
    }
    parser.report
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// Reduction percentage, rounded to 1dp, guarding divide-by-zero.
fn pct(saved: i64, before: i64) -> f64 {
    if before > 0 {
        (saved as f64 / before as f64 * 1000.0).round() / 10.0
    } else {
        0.0
    }
}

fn percentile(data: &[f64], pct: f64) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut sorted = data.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let index = (sorted.len() - 1) as f64 * pct;
    let lower = index as usize;
    let upper = lower + 1;
    let weight = index - lower as f64;
    if upper < sorted.len() {
        sorted[lower] * (1.0 - weight) + sorted[upper] * weight
    } else {
        sorted[lower]
    }
}

/// Throughput figures for one window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ThroughputStats {
    pub input_wall_clock: f64,
    pub input_active_p50: f64,
    pub input_active_p95: f64,
    pub compression_p50: f64,
    pub compression_p95: f64,
    pub forward_p50: f64,
    pub forward_p95: f64,
    pub generation_p50: f64,
    pub generation_p95: f64,
}

/// Rolling (whole window) and current (last 5 minutes) throughput.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Throughput {
    pub rolling: ThroughputStats,
    pub current: ThroughputStats,
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn throughput_stats(records: &[&PerfRecord], window_seconds: f64) -> ThroughputStats {
    if records.is_empty() {
        return ThroughputStats::default();
    }

    let total_tokens_before: i64 = records.iter().map(|r| r.tokens_before).sum();
    let input_wall = if window_seconds > 0.0 {
        total_tokens_before as f64 / window_seconds
    } else {
        0.0
    };

    let mut input_active_rates = Vec::new();
    let mut compression_rates = Vec::new();
    let mut forward_rates = Vec::new();
    let mut generation_rates = Vec::new();
    for r in records {
        if r.total_ms > 0.0 {
            input_active_rates.push(r.tokens_before as f64 / (r.total_ms / 1000.0));
            forward_rates.push(r.tokens_after as f64 / (r.total_ms / 1000.0));
        }
        let duration_ms = r
            .stages
            .get("compression_first_stage")
            .or_else(|| r.stages.get("compression"));
        if let Some(&d) = duration_ms {
            if d > 0.0 {
                compression_rates.push(r.tokens_before as f64 / (d / 1000.0));
            }
        }
        if r.tokens_out > 0 {
            let mut duration_ms = r.total_ms;
            if r.ttfb_ms > 0.0 && r.total_ms > r.ttfb_ms {
                duration_ms = r.total_ms - r.ttfb_ms;
            }
            if duration_ms > 0.0 {
                generation_rates.push(r.tokens_out as f64 / (duration_ms / 1000.0));
            }
        }
    }

    ThroughputStats {
        input_wall_clock: round2(input_wall),
        input_active_p50: round2(percentile(&input_active_rates, 0.5)),
        input_active_p95: round2(percentile(&input_active_rates, 0.95)),
        compression_p50: round2(percentile(&compression_rates, 0.5)),
        compression_p95: round2(percentile(&compression_rates, 0.95)),
        forward_p50: round2(percentile(&forward_rates, 0.5)),
        forward_p95: round2(percentile(&forward_rates, 0.95)),
        generation_p50: round2(percentile(&generation_rates, 0.5)),
        generation_p95: round2(percentile(&generation_rates, 0.95)),
    }
}

/// Rolling and 5-minute-current throughput from PERF timestamps.
pub fn calculate_throughput(report: &PerfReport) -> Throughput {
    let parsed: Vec<(&PerfRecord, NaiveDateTime)> = report
        .perf_records
        .iter()
        .filter_map(|r| parse_log_ts(&r.timestamp).map(|ts| (r, ts)))
        .collect();
    if parsed.is_empty() {
        return Throughput::default();
    }

    let oldest = parsed.iter().map(|(_, ts)| *ts).min().unwrap();
    let newest = parsed.iter().map(|(_, ts)| *ts).max().unwrap();
    let window_seconds = ((newest - oldest).num_milliseconds() as f64 / 1000.0).max(1.0);

    let all: Vec<&PerfRecord> = report.perf_records.iter().collect();
    let rolling = throughput_stats(&all, window_seconds);

    let cutoff_5m = newest - ChronoDuration::minutes(5);
    let current_pairs: Vec<&(&PerfRecord, NaiveDateTime)> =
        parsed.iter().filter(|(_, ts)| *ts >= cutoff_5m).collect();
    let current = if current_pairs.is_empty() {
        ThroughputStats::default()
    } else {
        let cur_oldest = current_pairs.iter().map(|(_, ts)| *ts).min().unwrap();
        let cur_window = ((newest - cur_oldest).num_milliseconds() as f64 / 1000.0).max(1.0);
        let cur_records: Vec<&PerfRecord> = current_pairs.iter().map(|(r, _)| *r).collect();
        throughput_stats(&cur_records, cur_window)
    };

    Throughput { rolling, current }
}

/// Actionable recommendations from the report data.
pub fn generate_recommendations(report: &PerfReport) -> Vec<String> {
    let mut recs = Vec::new();

    if !report.perf_records.is_empty() {
        let cache_recs: Vec<&PerfRecord> = report
            .perf_records
            .iter()
            .filter(|r| r.cache_read + r.cache_write > 0)
            .collect();
        if !cache_recs.is_empty() {
            let total_cr: i64 = cache_recs.iter().map(|r| r.cache_read).sum();
            let total_cw: i64 = cache_recs.iter().map(|r| r.cache_write).sum();
            if total_cw as f64 > total_cr as f64 * 1.5 {
                recs.push(
                    "Cache prefix unstable — compression decisions may be flipping across turns \
                     due to adaptive min_ratio threshold"
                        .to_string(),
                );
            }
            if cache_recs.len() >= 5 {
                let first5 = &cache_recs[..5];
                let early_ratio = first5.iter().map(|r| r.cache_read).sum::<i64>() as f64
                    / (first5.iter().map(|r| r.cache_write).sum::<i64>().max(1)) as f64;
                if early_ratio < 0.5 {
                    recs.push(
                        "First 5 turns have very low cache hit ratio — consider pinning \
                         compression decisions for prefix stability"
                            .to_string(),
                    );
                }
            }
        }

        let slow = report
            .perf_records
            .iter()
            .filter(|r| r.optimization_ms > 500.0)
            .count();
        if slow as f64 > report.perf_records.len() as f64 * 0.2 {
            recs.push(format!(
                "{slow} requests took >500ms for optimization — consider reducing transform pipeline"
            ));
        }
    }

    if !report.router_records.is_empty() {
        let total_excluded: i64 = report.router_records.iter().map(|r| r.excluded).sum();
        let total_compressed: i64 = report.router_records.iter().map(|r| r.compressed).sum();
        if total_excluded > 0 && total_compressed > 0 && total_excluded > total_compressed * 3 {
            recs.push(
                "Read/Glob outputs are majority of messages but excluded — compress stale reads \
                 (>10 turns old) for significant savings"
                    .to_string(),
            );
        }
    }

    if let Some(latest) = report.toin_records.last() {
        if latest.retrieval_rate == 0.0 && latest.compressions > 100 {
            recs.push(format!(
                "TOIN has 0% retrieval rate with {} compressions — review CCR integration",
                commafy(latest.compressions)
            ));
        }
    }

    for tr in &report.transform_records {
        if tr.name == "cache_aligner" && tr.tokens_saved < 10 {
            recs.push(
                "cache_aligner saving <10 tokens — consider disabling (system prompt likely has \
                 no dynamic content)"
                    .to_string(),
            );
            break;
        }
    }

    recs
}

// ---------------------------------------------------------------------------
// Context-tool (RTK) lifetime savings — its own counter never reaches
// proxy.log, so `headroom perf` surfaces it separately or it stays invisible.
// ---------------------------------------------------------------------------

/// Lifetime savings reported by the CLI context tool (RTK).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CliFiltering {
    pub tool: String,
    pub label: String,
    pub tokens_saved: i64,
    pub commands: i64,
    pub savings_pct: f64,
}

fn first_int(summary: &serde_json::Value, keys: &[&str]) -> i64 {
    for key in keys {
        if let Some(v) = summary.get(*key) {
            if let Some(n) = v.as_i64() {
                return n;
            }
            if let Some(f) = v.as_f64() {
                return f as i64;
            }
        }
    }
    0
}

fn first_float(summary: &serde_json::Value, keys: &[&str]) -> f64 {
    for key in keys {
        if let Some(f) = summary.get(*key).and_then(|v| v.as_f64()) {
            return f;
        }
    }
    0.0
}

/// Locate the rtk binary: PATH first, then the Headroom-managed install.
fn rtk_path() -> Option<PathBuf> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("rtk");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let managed = paths::workspace_dir().join("bin").join("rtk");
    if managed.is_file() {
        return Some(managed);
    }
    None
}

/// Best-effort lifetime savings from `rtk gain --format json`. Returns `None`
/// when RTK isn't installed, another context tool is selected, the subprocess
/// fails, or lifetime savings are zero — the report degrades to proxy-only.
pub fn context_tool_lifetime_savings() -> Option<CliFiltering> {
    let tool = std::env::var("HEADROOM_CONTEXT_TOOL").unwrap_or_default();
    let normalized = tool.trim().to_lowercase().replace('_', "-");
    if normalized == "lean-ctx" || normalized == "leanctx" {
        return None; // lean-ctx stats reading is not ported.
    }

    let rtk = rtk_path()?;
    let mut cmd = std::process::Command::new(rtk);
    cmd.arg("gain");
    if std::env::var("HEADROOM_RTK_GAIN_SCOPE")
        .map(|v| v.trim().eq_ignore_ascii_case("project"))
        .unwrap_or(false)
    {
        cmd.arg("--project");
    }
    cmd.args(["--format", "json"]);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let summary = data.get("summary")?;

    let input_tokens = first_int(
        summary,
        &[
            "total_input",
            "total_input_tokens",
            "input_tokens",
            "tokens_input",
            "totalBefore",
        ],
    );
    let output_tokens = first_int(
        summary,
        &[
            "total_output",
            "total_output_tokens",
            "output_tokens",
            "tokens_output",
            "totalAfter",
        ],
    );
    let mut tokens_saved = first_int(
        summary,
        &[
            "total_saved",
            "tokens_saved",
            "total_tokens_saved",
            "saved_tokens",
            "totalSaved",
        ],
    );
    if tokens_saved <= 0 && input_tokens > 0 && output_tokens >= 0 {
        tokens_saved = (input_tokens - output_tokens).max(0);
    }
    if tokens_saved <= 0 {
        return None;
    }
    let mut savings_pct = first_float(
        summary,
        &[
            "avg_savings_pct",
            "average_savings_pct",
            "savings_pct",
            "savings_percent",
            "avgSavingsPct",
        ],
    );
    let effective_input = if input_tokens <= 0 {
        tokens_saved + output_tokens.max(0)
    } else {
        input_tokens
    };
    if savings_pct <= 0.0 && effective_input > 0 {
        savings_pct = tokens_saved as f64 / effective_input as f64 * 100.0;
    }
    let commands = first_int(
        summary,
        &[
            "total_commands",
            "commands",
            "command_count",
            "totalCommandCount",
        ],
    );

    Some(CliFiltering {
        tool: "rtk".to_string(),
        label: "RTK".to_string(),
        tokens_saved,
        commands,
        savings_pct: (savings_pct * 10.0).round() / 10.0,
    })
}

fn cli_filtering_report_lines(cli: Option<&CliFiltering>) -> Vec<String> {
    let Some(cli) = cli else {
        return Vec::new();
    };
    vec![
        format!("{} CLI Filtering (lifetime, all-time)", cli.label),
        "-".repeat(40),
        format!(
            "  Tokens saved:  {} ({:.1}%)",
            commafy(cli.tokens_saved),
            cli.savings_pct
        ),
        format!("  Commands:      {}", commafy(cli.commands)),
        format!(
            "  Note: {}'s own lifetime counter — not limited to the --hours window.",
            cli.label
        ),
        String::new(),
    ]
}

// ---------------------------------------------------------------------------
// Text report
// ---------------------------------------------------------------------------

/// Format an integer with `,` thousands separators (Python `{:,}`).
fn commafy(value: i64) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    if negative {
        format!("-{out}")
    } else {
        out
    }
}

/// Python `{:g}`-ish for the window header: trim a trailing `.0`.
fn fmt_g(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e16 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Python `str(float)`-ish: whole floats keep one decimal (`0.0`, `12.0`).
fn fmt_py_float(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e16 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

/// Format a PerfReport into a human-readable string (mirrors Python's
/// `format_report`, minus the TOIN-highlights section).
pub fn format_report(report: &PerfReport, cli: Option<&CliFiltering>) -> String {
    let mut lines: Vec<String> = Vec::new();
    let cli_lines = cli_filtering_report_lines(cli);

    if report.perf_records.is_empty() && report.router_records.is_empty() {
        if !cli_lines.is_empty() {
            // RTK savings are independent of proxy logs — surface them even
            // when there is no proxy traffic in the window.
            lines.push(
                "No proxy performance data in ~/.headroom/logs/ for this window.".to_string(),
            );
            lines.push(String::new());
            lines.extend(cli_lines);
        } else {
            lines.push("No performance data found in ~/.headroom/logs/".to_string());
            lines.push(String::new());
            lines.push("Start the proxy to begin collecting data:".to_string());
            lines.push("  headroom proxy".to_string());
        }
        return lines.join("\n");
    }

    lines.push("Headroom Performance Report".to_string());
    lines.push("=".repeat(60));
    if let Some(hours) = report.requested_hours {
        let window_label = if report.window_all_data {
            "all data".to_string()
        } else {
            format!("last {}h", fmt_g(hours))
        };
        match (&report.oldest_kept_ts, &report.newest_kept_ts) {
            (Some(oldest), Some(newest)) => lines.push(format!(
                "Window: {window_label} (actual data: {} → {})",
                &oldest[..oldest.len().min(19)],
                &newest[..newest.len().min(19)]
            )),
            _ => lines.push(format!(
                "Window: {window_label} (no records found in window)"
            )),
        }
        if report.records_filtered_out > 0 {
            lines.push(format!(
                "Records outside window:  {} (filtered out — increase --hours to include them)",
                commafy(report.records_filtered_out)
            ));
        }
    }
    lines.push(String::new());

    let records = &report.perf_records;
    if !records.is_empty() {
        let total_before: i64 = records.iter().map(|r| r.tokens_before).sum();
        let total_after: i64 = records.iter().map(|r| r.tokens_after).sum();
        let total_saved: i64 = records.iter().map(|r| r.tokens_saved).sum();
        let pct = if total_before > 0 {
            total_saved as f64 / total_before as f64 * 100.0
        } else {
            0.0
        };

        lines.push(format!("Requests:     {}", records.len()));
        lines.push(format!(
            "Tokens:       {} -> {} ({pct:.1}% reduction)",
            commafy(total_before),
            commafy(total_after)
        ));
        lines.push(format!("Total saved:  {} tokens", commafy(total_saved)));
        lines.push(String::new());

        let mut by_model: std::collections::BTreeMap<&str, Vec<&PerfRecord>> = Default::default();
        for r in records {
            by_model.entry(r.model.as_str()).or_default().push(r);
        }
        lines.push("Per-Model Breakdown".to_string());
        lines.push("-".repeat(40));
        for (model, model_recs) in &by_model {
            let m_saved: i64 = model_recs.iter().map(|r| r.tokens_saved).sum();
            let m_before: i64 = model_recs.iter().map(|r| r.tokens_before).sum();
            let m_pct = if m_before > 0 {
                m_saved as f64 / m_before as f64 * 100.0
            } else {
                0.0
            };
            let list_price = get_list_price(model);
            let price_str = match list_price {
                Some(p) => format!("${p:.2}/MTok"),
                None => "unknown".to_string(),
            };
            let est_str = match list_price {
                Some(p) => format!("  ~${:.2} at list price", m_saved as f64 * p / 1_000_000.0),
                None => String::new(),
            };
            lines.push(format!(
                "  {model}: {} reqs, {} tokens saved ({m_pct:.0}%), list price {price_str}{est_str}",
                model_recs.len(),
                commafy(m_saved)
            ));
        }
        lines.push("  * Actual bill savings depend on provider caching behavior".to_string());
        lines.push(String::new());

        let cache_records: Vec<&PerfRecord> = records
            .iter()
            .filter(|r| r.cache_read + r.cache_write > 0)
            .collect();
        if !cache_records.is_empty() {
            lines.push("Cache Performance".to_string());
            lines.push("-".repeat(40));
            let total_cr: i64 = cache_records.iter().map(|r| r.cache_read).sum();
            let total_cw: i64 = cache_records.iter().map(|r| r.cache_write).sum();
            let total_cache = total_cr + total_cw;
            let hit_pct = if total_cache > 0 {
                total_cr as f64 / total_cache as f64 * 100.0
            } else {
                0.0
            };
            lines.push(format!("  Cache read:    {} tokens", commafy(total_cr)));
            lines.push(format!("  Cache write:   {} tokens", commafy(total_cw)));
            lines.push(format!("  Hit rate:      {hit_pct:.1}%"));

            let unstable = cache_records
                .iter()
                .filter(|r| r.cache_write > r.cache_read * 2)
                .count();
            if unstable > 0 {
                lines.push(format!(
                    "  Unstable:      {unstable}/{} requests had cache_write > 2x cache_read",
                    cache_records.len()
                ));
            }

            if cache_records.len() >= 10 {
                let first5 = &cache_records[..5];
                let last5 = &cache_records[cache_records.len() - 5..];
                let first5_cr: i64 = first5.iter().map(|r| r.cache_read).sum();
                let first5_cw: i64 = first5.iter().map(|r| r.cache_write).sum();
                let last5_cr: i64 = last5.iter().map(|r| r.cache_read).sum();
                let last5_cw: i64 = last5.iter().map(|r| r.cache_write).sum();
                lines.push(format!(
                    "  First 5 avg:   read={} write={}",
                    commafy(first5_cr / 5),
                    commafy(first5_cw / 5)
                ));
                lines.push(format!(
                    "  Last 5 avg:    read={} write={}",
                    commafy(last5_cr / 5),
                    commafy(last5_cw / 5)
                ));
                if last5_cr > first5_cr * 2 {
                    lines.push("  -> Cache stabilizing over conversation lifetime".to_string());
                } else if first5_cw > first5_cr * 3 {
                    lines.push(
                        "  ! Early turns have poor cache hits — compression decisions may be \
                         flipping"
                            .to_string(),
                    );
                }
            }
            lines.push(String::new());
        }

        let opt_times: Vec<f64> = records
            .iter()
            .filter(|r| r.optimization_ms > 0.0)
            .map(|r| r.optimization_ms)
            .collect();
        if !opt_times.is_empty() {
            let avg_opt = opt_times.iter().sum::<f64>() / opt_times.len() as f64;
            let max_opt = opt_times.iter().cloned().fold(f64::MIN, f64::max);
            lines.push("Optimization Overhead".to_string());
            lines.push("-".repeat(40));
            lines.push(format!("  Average:  {avg_opt:.0}ms"));
            lines.push(format!("  Max:      {max_opt:.0}ms"));
            let slow = opt_times.iter().filter(|t| **t > 500.0).count();
            if slow > 0 {
                lines.push(format!("  >500ms:   {slow} requests"));
            }
            lines.push(String::new());
        }

        let tp = calculate_throughput(report);
        let (rolling, current) = (&tp.rolling, &tp.current);
        if rolling.input_wall_clock > 0.0 || rolling.input_active_p50 > 0.0 {
            lines.push("Throughput".to_string());
            lines.push("-".repeat(40));
            lines.push(format!(
                "  Input (wall-clock):   {:.1} tok/s (current: {:.1} tok/s)",
                rolling.input_wall_clock, current.input_wall_clock
            ));
            lines.push(format!(
                "  Input (active p50/95): {:.1} / {:.1} tok/s (current: {:.1} / {:.1} tok/s)",
                rolling.input_active_p50,
                rolling.input_active_p95,
                current.input_active_p50,
                current.input_active_p95
            ));
            if rolling.compression_p50 > 0.0 {
                lines.push(format!(
                    "  Compression (p50/95):  {:.1} / {:.1} tok/s (current: {:.1} / {:.1} tok/s)",
                    rolling.compression_p50,
                    rolling.compression_p95,
                    current.compression_p50,
                    current.compression_p95
                ));
            }
            lines.push(format!(
                "  Forward (p50/95):      {:.1} / {:.1} tok/s (current: {:.1} / {:.1} tok/s)",
                rolling.forward_p50, rolling.forward_p95, current.forward_p50, current.forward_p95
            ));
            if rolling.generation_p50 > 0.0 {
                lines.push(format!(
                    "  Generation (p50/95):   {:.1} / {:.1} tok/s (current: {:.1} / {:.1} tok/s)",
                    rolling.generation_p50,
                    rolling.generation_p95,
                    current.generation_p50,
                    current.generation_p95
                ));
            }
            lines.push(String::new());
        }

        let msg_counts: Vec<i64> = records
            .iter()
            .filter(|r| r.num_messages > 0)
            .map(|r| r.num_messages)
            .collect();
        if !msg_counts.is_empty() {
            lines.push("Conversation Size".to_string());
            lines.push("-".repeat(40));
            lines.push(format!("  Min msgs:  {}", msg_counts.iter().min().unwrap()));
            lines.push(format!("  Max msgs:  {}", msg_counts.iter().max().unwrap()));
            lines.push(format!(
                "  Avg msgs:  {}",
                msg_counts.iter().sum::<i64>() / msg_counts.len() as i64
            ));
            lines.push(String::new());
        }
    }

    if !report.transform_records.is_empty() {
        lines.push("Transform Effectiveness".to_string());
        lines.push("-".repeat(40));
        let mut by_name: HashMap<&str, Vec<&TransformRecord>> = HashMap::new();
        for tr in &report.transform_records {
            by_name.entry(tr.name.as_str()).or_default().push(tr);
        }
        let mut entries: Vec<(&str, Vec<&TransformRecord>)> = by_name.into_iter().collect();
        entries.sort_by_key(|(_, recs)| -recs.iter().map(|r| r.tokens_saved).sum::<i64>());
        for (name, recs) in entries {
            let total_s: i64 = recs.iter().map(|r| r.tokens_saved).sum();
            let total_b: i64 = recs.iter().map(|r| r.tokens_before).sum();
            let avg_pct = if total_b > 0 {
                total_s as f64 / total_b as f64 * 100.0
            } else {
                0.0
            };
            lines.push(format!(
                "  {name}: {avg_pct:.1}% avg reduction, {} uses, {} saved",
                recs.len(),
                commafy(total_s)
            ));
        }
        lines.push(String::new());
    }

    if !report.router_records.is_empty() {
        lines.push("Content Router Routing".to_string());
        lines.push("-".repeat(40));
        let total_compressed: i64 = report.router_records.iter().map(|r| r.compressed).sum();
        let total_excluded: i64 = report.router_records.iter().map(|r| r.excluded).sum();
        let total_skipped: i64 = report.router_records.iter().map(|r| r.skipped).sum();
        let total_unchanged: i64 = report.router_records.iter().map(|r| r.unchanged).sum();
        let total_all = total_compressed + total_excluded + total_skipped + total_unchanged;
        if total_all > 0 {
            let share = |n: i64| n as f64 / total_all as f64 * 100.0;
            lines.push(format!(
                "  Compressed:  {total_compressed} ({:.0}%)",
                share(total_compressed)
            ));
            lines.push(format!(
                "  Excluded:    {total_excluded} ({:.0}%) — Read/Glob outputs",
                share(total_excluded)
            ));
            lines.push(format!(
                "  Skipped:     {total_skipped} ({:.0}%) — <50 words",
                share(total_skipped)
            ));
            lines.push(format!(
                "  Unchanged:   {total_unchanged} ({:.0}%) — ratio too high",
                share(total_unchanged)
            ));
        }
        if total_excluded > total_compressed * 3 {
            lines.push(
                "  ! Excluded tools dominate — consider compressing stale Read outputs".to_string(),
            );
        }
        lines.push(String::new());
    }

    if let Some(latest) = report.toin_records.last() {
        lines.push("TOIN Learning".to_string());
        lines.push("-".repeat(40));
        lines.push(format!("  Patterns:     {}", latest.patterns));
        lines.push(format!("  Compressions: {}", commafy(latest.compressions)));
        lines.push(format!(
            "  Retrievals:   {} ({}%)",
            latest.retrievals,
            fmt_py_float(latest.retrieval_rate)
        ));
        if latest.retrieval_rate == 0.0 && latest.compressions > 100 {
            lines.push("  ! 0% retrieval rate — TOIN learning but never used".to_string());
        }
        lines.push(String::new());
    }

    let recommendations = generate_recommendations(report);
    if !recommendations.is_empty() {
        lines.push("Recommendations".to_string());
        lines.push("-".repeat(40));
        for (i, rec) in recommendations.iter().enumerate() {
            lines.push(format!("  {}. {rec}", i + 1));
        }
        lines.push(String::new());
    }

    lines.extend(cli_lines);

    lines.push(format!(
        "Log files: {} | Lines parsed: {}",
        report.log_files_read,
        commafy(report.total_lines_parsed)
    ));
    lines.push(format!("Log dir: {}", log_dir().display()));

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Machine-readable views (JSON / CSV)
// ---------------------------------------------------------------------------

/// Column order for the per-record (`--raw`) machine output.
pub const PERF_RECORD_FIELDS: &[&str] = &[
    "timestamp",
    "request_id",
    "model",
    "client",
    "num_messages",
    "tokens_before",
    "tokens_after",
    "tokens_saved",
    "cache_read",
    "cache_write",
    "cache_hit_pct",
    "optimization_ms",
    "transforms",
    "total_ms",
    "tokens_out",
    "ttfb_ms",
    "stages",
];

/// Aggregate a report into a JSON-serialisable summary (mirrors Python's
/// `build_perf_summary`).
pub fn build_perf_summary(report: &PerfReport, cli: Option<&CliFiltering>) -> serde_json::Value {
    let records = &report.perf_records;
    let total_before: i64 = records.iter().map(|r| r.tokens_before).sum();
    let total_after: i64 = records.iter().map(|r| r.tokens_after).sum();
    let total_saved: i64 = records.iter().map(|r| r.tokens_saved).sum();

    let total_cr: i64 = records.iter().map(|r| r.cache_read).sum();
    let total_cw: i64 = records.iter().map(|r| r.cache_write).sum();
    let total_cache = total_cr + total_cw;
    let cache_hit_pct = if total_cache > 0 {
        (total_cr as f64 / total_cache as f64 * 1000.0).round() / 10.0
    } else {
        0.0
    };

    let mut by_model_groups: std::collections::BTreeMap<&str, Vec<&PerfRecord>> =
        Default::default();
    for r in records {
        by_model_groups.entry(r.model.as_str()).or_default().push(r);
    }
    let by_model: Vec<serde_json::Value> = by_model_groups
        .iter()
        .map(|(model, recs)| {
            let m_before: i64 = recs.iter().map(|r| r.tokens_before).sum();
            let m_after: i64 = recs.iter().map(|r| r.tokens_after).sum();
            let m_saved: i64 = recs.iter().map(|r| r.tokens_saved).sum();
            serde_json::json!({
                "model": model,
                "requests": recs.len(),
                "tokens_before": m_before,
                "tokens_after": m_after,
                "tokens_saved": m_saved,
                "savings_pct": pct(m_saved, m_before),
                "list_price_per_mtok": get_list_price(model),
            })
        })
        .collect();

    let mut by_transform_groups: HashMap<&str, Vec<&TransformRecord>> = HashMap::new();
    for tr in &report.transform_records {
        by_transform_groups
            .entry(tr.name.as_str())
            .or_default()
            .push(tr);
    }
    let mut transform_entries: Vec<(&str, Vec<&TransformRecord>)> =
        by_transform_groups.into_iter().collect();
    transform_entries.sort_by_key(|(_, recs)| -recs.iter().map(|r| r.tokens_saved).sum::<i64>());
    let by_transform: Vec<serde_json::Value> = transform_entries
        .iter()
        .map(|(name, recs)| {
            let t_before: i64 = recs.iter().map(|r| r.tokens_before).sum();
            let t_saved: i64 = recs.iter().map(|r| r.tokens_saved).sum();
            serde_json::json!({
                "transform": name,
                "uses": recs.len(),
                "tokens_before": t_before,
                "tokens_saved": t_saved,
                "savings_pct": pct(t_saved, t_before),
            })
        })
        .collect();

    serde_json::json!({
        "window_hours": report.requested_hours,
        "actual_window": {
            "oldest": report.oldest_kept_ts,
            "newest": report.newest_kept_ts,
        },
        "records_filtered_out": report.records_filtered_out,
        "total_requests": records.len(),
        "total_tokens_before": total_before,
        "total_tokens_after": total_after,
        "tokens_saved": total_saved,
        "savings_pct": pct(total_saved, total_before),
        "cache_read_tokens": total_cr,
        "cache_write_tokens": total_cw,
        "cache_hit_pct": cache_hit_pct,
        "by_model": by_model,
        "by_transform": by_transform,
        "throughput": calculate_throughput(report),
        "log_files_read": report.log_files_read,
        "total_lines_parsed": report.total_lines_parsed,
        "cli_filtering": cli.map(|c| serde_json::json!({
            "tool": c.tool,
            "label": c.label,
            "tokens_saved": c.tokens_saved,
            "commands": c.commands,
            "savings_pct": c.savings_pct,
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERF_LINE: &str = "2026-06-10 10:00:00,000 - headroom.proxy - INFO - [hr_smoke_claude] \
        PERF model=claude-sonnet-4 msgs=3 tok_before=1000 tok_after=80 tok_saved=920 \
        cache_read=0 cache_write=0 cache_hit_pct=0 opt_ms=1 transforms=agent90_smoke client=claude";

    fn parse_lines(lines: &[&str], hours: f64) -> PerfReport {
        let mut parser = Parser::new(hours);
        for line in lines {
            parser.feed_line(line);
        }
        parser.report
    }

    #[test]
    fn parse_kv_handles_spaced_transforms() {
        let kv = parse_kv(
            "model=m msgs=3 tok_before=10 transforms=router:excluded:tool*32 read_lifecycle:stale*17",
        );
        assert_eq!(kv["model"], "m");
        assert_eq!(kv["tok_before"], "10");
        assert_eq!(
            kv["transforms"],
            "router:excluded:tool*32 read_lifecycle:stale*17"
        );
    }

    #[test]
    fn parse_perf_line_fields() {
        let report = parse_lines(&[PERF_LINE], 0.0);
        assert_eq!(report.perf_records.len(), 1);
        let r = &report.perf_records[0];
        assert_eq!(r.request_id, "hr_smoke_claude");
        assert_eq!(r.model, "claude-sonnet-4");
        assert_eq!(r.client, "claude");
        assert_eq!(r.num_messages, 3);
        assert_eq!(r.tokens_before, 1000);
        assert_eq!(r.tokens_after, 80);
        assert_eq!(r.tokens_saved, 920);
        assert_eq!(r.transforms, vec!["agent90_smoke".to_string()]);
        assert!(report.window_all_data);
        assert_eq!(
            report.oldest_kept_ts.as_deref(),
            Some("2026-06-10 10:00:00,000")
        );
    }

    #[test]
    fn perf_transforms_star_format_strips_counts() {
        let line = "2026-06-10 10:00:00,000 - headroom.proxy - INFO - [rid] PERF model=m \
                    msgs=1 tok_before=1 tok_after=1 tok_saved=0 transforms=router:excluded:tool*32 read_lifecycle:stale*17";
        let report = parse_lines(&[line], 0.0);
        assert_eq!(
            report.perf_records[0].transforms,
            vec![
                "router:excluded:tool".to_string(),
                "read_lifecycle:stale".to_string()
            ]
        );
    }

    #[test]
    fn stage_timings_attach_to_perf_record() {
        let stage_line = "2026-06-10 09:59:59,900 - headroom.proxy - INFO - [rid1] STAGE_TIMINGS \
            {\"event\": \"stage_timings\", \"stages\": {\"compression\": 50.0, \"noop\": null}}";
        let perf_line = "2026-06-10 10:00:00,000 - headroom.proxy - INFO - [rid1] PERF model=m \
            msgs=1 tok_before=100 tok_after=50 tok_saved=50 total_ms=200 transforms=none";
        let report = parse_lines(&[stage_line, perf_line], 0.0);
        assert_eq!(report.perf_records.len(), 1);
        assert_eq!(
            report.perf_records[0].stages.get("compression"),
            Some(&50.0)
        );
        assert!(!report.perf_records[0].stages.contains_key("noop"));
    }

    #[test]
    fn window_filter_drops_old_records() {
        // A record from 2020 is far outside a 1-hour window.
        let old = "2020-01-01 00:00:00,000 - headroom.proxy - INFO - [rid] PERF model=m \
                   msgs=1 tok_before=1 tok_after=1 tok_saved=0 transforms=none";
        let report = parse_lines(&[old], 1.0);
        assert!(report.perf_records.is_empty());
        assert_eq!(report.records_filtered_out, 1);
        assert!(!report.window_all_data);
    }

    #[test]
    fn router_and_transform_and_toin_lines() {
        let router = "2026-06-10 10:00:01,000 - headroom.proxy - INFO - content_router: 51 msgs — \
                      12 compressed, 30 excluded, 5 skipped, 4 unchanged, 51 content blocks";
        let transform = "2026-06-10 10:00:02,000 - headroom.proxy - INFO - \
                         Transform content_router: 52503 -> 26006 tokens (saved 26497)";
        let toin = "2026-06-10 10:00:03,000 - headroom.proxy - INFO - TOIN: 105 patterns, \
                    3837 compressions, 0 retrievals, 0.0% retrieval rate";
        let report = parse_lines(&[router, transform, toin], 0.0);
        let rr = &report.router_records[0];
        assert_eq!(
            (
                rr.num_messages,
                rr.compressed,
                rr.excluded,
                rr.skipped,
                rr.unchanged,
                rr.content_blocks
            ),
            (51, 12, 30, 5, 4, 51)
        );
        let tr = &report.transform_records[0];
        assert_eq!(
            (tr.tokens_before, tr.tokens_after, tr.tokens_saved),
            (52503, 26006, 26497)
        );
        let to = &report.toin_records[0];
        assert_eq!(
            (to.patterns, to.compressions, to.retrievals),
            (105, 3837, 0)
        );
        assert_eq!(to.retrieval_rate, 0.0);
    }

    #[test]
    fn percentile_interpolates() {
        let data = vec![10.0, 20.0, 30.0, 40.0];
        assert_eq!(percentile(&data, 0.5), 25.0);
        assert_eq!(percentile(&data, 0.0), 10.0);
        assert_eq!(percentile(&data, 1.0), 40.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
    }

    #[test]
    fn throughput_from_records() {
        // Two records 10s apart, 1000 tokens_before each → 200 tok/s wall clock.
        let l1 = "2026-06-10 10:00:00,000 - x - INFO - [r1] PERF model=m msgs=1 tok_before=1000 \
                  tok_after=500 tok_saved=500 total_ms=1000 transforms=none";
        let l2 = "2026-06-10 10:00:10,000 - x - INFO - [r2] PERF model=m msgs=1 tok_before=1000 \
                  tok_after=500 tok_saved=500 total_ms=1000 transforms=none";
        let report = parse_lines(&[l1, l2], 0.0);
        let tp = calculate_throughput(&report);
        assert_eq!(tp.rolling.input_wall_clock, 200.0);
        assert_eq!(tp.rolling.input_active_p50, 1000.0);
        assert_eq!(tp.rolling.forward_p50, 500.0);
        // Both records are within 5 minutes of the newest → current == rolling.
        assert_eq!(tp.current.input_wall_clock, 200.0);
    }

    #[test]
    fn format_report_empty() {
        let report = PerfReport::default();
        let text = format_report(&report, None);
        assert!(text.contains("No performance data found"));
        assert!(text.contains("headroom proxy"));
    }

    #[test]
    fn format_report_cli_filtering_section() {
        let cli = CliFiltering {
            tool: "rtk".to_string(),
            label: "RTK".to_string(),
            tokens_saved: 853_117_835,
            commands: 56_819,
            savings_pct: 92.4,
        };
        // Empty report + RTK stats → RTK-only variant.
        let text = format_report(&PerfReport::default(), Some(&cli));
        assert!(text.contains("No proxy performance data in ~/.headroom/logs/ for this window."));
        assert!(text.contains("RTK CLI Filtering (lifetime, all-time)"));
        assert!(text.contains("  Tokens saved:  853,117,835 (92.4%)"));
        assert!(text.contains("  Commands:      56,819"));
        // Non-empty report → section sits before the footer.
        let report = parse_lines(&[PERF_LINE], 0.0);
        let text = format_report(&report, Some(&cli));
        let section = text.find("RTK CLI Filtering").unwrap();
        let footer = text.find("Log files:").unwrap();
        assert!(section < footer);
        // TOIN retrieval rate keeps Python float formatting.
        let toin = "2026-06-10 10:00:03,000 - x - INFO - TOIN: 105 patterns, 3837 compressions, \
                    0 retrievals, 0.0% retrieval rate";
        let report = parse_lines(&[PERF_LINE, toin], 0.0);
        assert!(format_report(&report, None).contains("  Retrievals:   0 (0.0%)"));
    }

    #[test]
    fn format_report_sections() {
        let report = parse_lines(&[PERF_LINE], 0.0);
        let text = format_report(&report, None);
        assert!(text.contains("Headroom Performance Report"));
        assert!(text
            .contains("Window: all data (actual data: 2026-06-10 10:00:00 → 2026-06-10 10:00:00)"));
        assert!(text.contains("Requests:     1"));
        assert!(text.contains("Tokens:       1,000 -> 80 (92.0% reduction)"));
        assert!(text.contains("Total saved:  920 tokens"));
        assert!(text.contains("Per-Model Breakdown"));
        // claude-sonnet-4 resolves in the vendored pricing table at $3/MTok.
        assert!(text.contains("claude-sonnet-4: 1 reqs, 920 tokens saved (92%), list price $3.00/MTok  ~$0.00 at list price"));
        assert!(text.contains("Log files: 0 | Lines parsed: 1"));
    }

    #[test]
    fn format_report_unknown_model_price() {
        let line = "2026-06-10 10:00:00,000 - x - INFO - [r] PERF model=mystery msgs=1 \
                    tok_before=100 tok_after=50 tok_saved=50 transforms=none";
        let report = parse_lines(&[line], 0.0);
        let text = format_report(&report, None);
        assert!(text.contains("list price unknown"));
    }

    #[test]
    fn recommendations_fire_on_signals() {
        // Unstable cache: write >> read.
        let line = "2026-06-10 10:00:00,000 - x - INFO - [r] PERF model=m msgs=1 tok_before=100 \
                    tok_after=50 tok_saved=50 cache_read=10 cache_write=100 transforms=none";
        let toin = "2026-06-10 10:00:03,000 - x - INFO - TOIN: 105 patterns, 3837 compressions, \
                    0 retrievals, 0.0% retrieval rate";
        let report = parse_lines(&[line, toin], 0.0);
        let recs = generate_recommendations(&report);
        assert!(recs.iter().any(|r| r.contains("Cache prefix unstable")));
        assert!(recs
            .iter()
            .any(|r| r.contains("TOIN has 0% retrieval rate with 3,837 compressions")));
    }

    #[test]
    fn summary_shape_and_totals() {
        let report = parse_lines(&[PERF_LINE], 0.0);
        let summary = build_perf_summary(&report, None);
        assert_eq!(summary["total_requests"], 1);
        assert_eq!(summary["total_tokens_before"], 1000);
        assert_eq!(summary["tokens_saved"], 920);
        assert_eq!(summary["savings_pct"], 92.0);
        assert_eq!(summary["by_model"][0]["model"], "claude-sonnet-4");
        assert_eq!(summary["by_model"][0]["savings_pct"], 92.0);
        assert!(summary["cli_filtering"].is_null());
    }

    #[test]
    fn parse_log_files_reads_rotated_logs() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("proxy.log"), format!("{PERF_LINE}\n")).unwrap();
        std::fs::write(logs.join("proxy.log.1"), format!("{PERF_LINE}\n")).unwrap();
        std::fs::write(logs.join("other.txt"), "ignored\n").unwrap();
        // Serialized via env var; no other test in this module touches it.
        std::env::set_var(paths::HEADROOM_WORKSPACE_DIR_ENV, dir.path());
        let report = parse_log_files(0.0);
        std::env::remove_var(paths::HEADROOM_WORKSPACE_DIR_ENV);
        assert_eq!(report.log_files_read, 2);
        assert_eq!(report.perf_records.len(), 2);
        assert_eq!(report.total_lines_parsed, 2);
    }
}
