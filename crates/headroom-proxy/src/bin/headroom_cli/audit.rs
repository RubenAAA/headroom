//! Read-opportunity audits over local transcripts (Rust port of
//! `headroom/audit/{reads,maturation,codex}.py`). Read-only: streams
//! `<root>/**/*.jsonl` and never modifies anything.
//!
//! Three analyses:
//! - `audit_reads` — sizes the addressable bytes for each Read compression
//!   mechanism on Claude Code transcripts (identical repeats, subset
//!   containment, write-readback, stale reads, line-number scaffolding,
//!   context residency, cache-death windows).
//! - `simulate_maturation` — Mechanism B (read maturation) simulation:
//!   re-read rates, never-touched-again share, quiesce-window coverage,
//!   at-risk edits.
//! - `audit_codex` — shell-based read patterns in Codex transcripts
//!   (cat/sed/head/tail, rtk wrappers).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex_lite::Regex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub const MIN_SIZE: i64 = 512; // matches ReadLifecycleConfig.min_size_bytes
pub const MATURE_FLOOR: i64 = 2048; // ReadMaturationConfig.min_size_bytes
const QUIESCE_CANDIDATES: [i64; 6] = [1, 2, 3, 5, 10, 25];

const LOCK_GENERATED: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "cargo.lock",
    "go.sum",
    "poetry.lock",
    "uv.lock",
    "gemfile.lock",
    "composer.lock",
];
const SOURCE_EXT: &[&str] = &[
    ".py", ".ts", ".tsx", ".js", ".jsx", ".rs", ".go", ".java", ".c", ".cpp", ".h", ".rb",
    ".swift", ".kt", ".scala", ".sh", ".zsh",
];
const DATA_EXT: &[&str] = &[".json", ".jsonl", ".csv", ".yaml", ".yml", ".toml", ".xml"];
const DOC_EXT: &[&str] = &[".md", ".rst", ".txt"];
const MUTATING_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

fn linenum_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*\d+\t").unwrap())
}

// ─── Shared helpers ──────────────────────────────────────────────────────

/// Ordered count map (first-touch insertion order, like a defaultdict) so
/// descending sorts keep Python's tie order.
fn bump(map: &mut Vec<(String, i64)>, key: &str, by: i64) {
    match map.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 += by,
        None => map.push((key.to_string(), by)),
    }
}

fn pairs_to_object(pairs: &[(String, i64)]) -> Value {
    Value::Object(pairs.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
}

/// Recursively walk for `*.jsonl` files, sorted (mirrors sorted glob).
fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Concatenated text of a tool_result content (str or list of text blocks).
fn block_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| {
                (b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .then(|| b.get("text").and_then(|t| t.as_str()).unwrap_or(""))
            })
            .collect(),
        _ => String::new(),
    }
}

fn get_str<'a>(map: &'a Map<String, Value>, key: &str) -> &'a str {
    map.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

/// `input.get("file_path") or input.get("path") or ""`.
fn file_path_of(input: &Map<String, Value>) -> String {
    let fp = get_str(input, "file_path");
    if !fp.is_empty() {
        return fp.to_string();
    }
    get_str(input, "path").to_string()
}

/// Epoch seconds from an ISO timestamp ("Z" or offset). Naive timestamps
/// are treated as UTC (Python uses local time; only gap *differences*
/// matter here, so the deviation is harmless).
fn parse_ts(line: &Value) -> Option<f64> {
    let ts = line.get("timestamp")?.as_str()?;
    if ts.is_empty() {
        return None;
    }
    let with_offset = ts.replace('Z', "+00:00");
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&with_offset) {
        return Some(dt.timestamp() as f64 + f64::from(dt.timestamp_subsec_micros()) / 1e6);
    }
    let naive = chrono::NaiveDateTime::parse_from_str(&with_offset, "%Y-%m-%dT%H:%M:%S").ok()?;
    Some(naive.and_utc().timestamp() as f64)
}

/// `sorted(xs)[int(len(xs) * p)]` (assumes `xs` pre-sorted).
fn pct_index(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        0
    } else {
        sorted[(sorted.len() as f64 * p) as usize]
    }
}

/// json.dumps(value, indent=2, sort_keys=True) — object keys sorted, with
/// Python's numeric ordering for the int-keyed quiesce maps.
pub fn py_dumps_sorted(value: &Value) -> String {
    fn sort(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                if !map.is_empty() && keys.iter().all(|k| k.parse::<i64>().is_ok()) {
                    keys.sort_by_key(|k| k.parse::<i64>().unwrap());
                } else {
                    keys.sort();
                }
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), sort(&map[k.as_str()])))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string_pretty(&sort(value)).unwrap_or_default()
}

// ─── audit-reads (Claude Code transcripts) ───────────────────────────────

/// Aggregated audit results. All byte figures are UTF-8 bytes of
/// tool_result content; tokens ≈ bytes / 4.
#[derive(Default)]
pub struct ReadAuditReport {
    pub sessions: i64,
    pub files_skipped: i64,
    pub tool_bytes: Vec<(String, i64)>,
    pub read_calls: i64,
    pub read_bytes: i64,
    pub read_calls_small: i64,
    pub dedup_identical_calls: i64,
    pub dedup_identical_bytes: i64,
    pub subset_calls: i64,
    pub subset_bytes: i64,
    pub write_readback_calls: i64,
    pub write_readback_bytes: i64,
    pub stale_calls: i64,
    pub stale_bytes: i64,
    pub linenum_overhead_bytes: i64,
    pub class_bytes: Vec<(String, i64)>,
    pub residency_median: i64,
    pub residency_p90: i64,
    pub residency_mean: f64,
    pub gaps_over_5m: i64,
    pub gaps_over_1h: i64,
    pub sessions_with_gap: i64,
    pub reads_per_file_max_median: i64,
    pub reads_per_file_max: i64,
}

impl ReadAuditReport {
    pub fn to_json_value(&self) -> Value {
        json!({
            "sessions": self.sessions,
            "files_skipped": self.files_skipped,
            "tool_bytes": pairs_to_object(&self.tool_bytes),
            "read_calls": self.read_calls,
            "read_bytes": self.read_bytes,
            "read_calls_small": self.read_calls_small,
            "dedup_identical_calls": self.dedup_identical_calls,
            "dedup_identical_bytes": self.dedup_identical_bytes,
            "subset_calls": self.subset_calls,
            "subset_bytes": self.subset_bytes,
            "write_readback_calls": self.write_readback_calls,
            "write_readback_bytes": self.write_readback_bytes,
            "stale_calls": self.stale_calls,
            "stale_bytes": self.stale_bytes,
            "linenum_overhead_bytes": self.linenum_overhead_bytes,
            "class_bytes": pairs_to_object(&self.class_bytes),
            "residency_median": self.residency_median,
            "residency_p90": self.residency_p90,
            "residency_mean": self.residency_mean,
            "gaps_over_5m": self.gaps_over_5m,
            "gaps_over_1h": self.gaps_over_1h,
            "sessions_with_gap": self.sessions_with_gap,
            "reads_per_file_max_median": self.reads_per_file_max_median,
            "reads_per_file_max": self.reads_per_file_max,
        })
    }
}

fn classify_path(p: &str) -> &'static str {
    let low = p.to_lowercase();
    let name = low.rsplit('/').next().unwrap_or("");
    if LOCK_GENERATED.contains(&name)
        || low.contains("/node_modules/")
        || low.contains("/dist/")
        || name.contains(".min.")
    {
        return "lock/generated/vendored";
    }
    if SOURCE_EXT.iter().any(|ext| name.ends_with(ext)) {
        return "source code";
    }
    if DOC_EXT.iter().any(|ext| name.ends_with(ext)) {
        return "docs/text";
    }
    if DATA_EXT.iter().any(|ext| name.ends_with(ext)) {
        return "data/config";
    }
    "other"
}

#[derive(Default)]
struct ReadsAgg {
    report: ReadAuditReport,
    tool_bytes: Vec<(String, i64)>,
    class_bytes: Vec<(String, i64)>,
    residency: Vec<i64>,
    reads_per_file_max: Vec<i64>,
}

fn audit_session(path: &Path, agg: &mut ReadsAgg) -> std::io::Result<()> {
    let text = String::from_utf8_lossy(&std::fs::read(path)?).into_owned();
    let r = &mut agg.report;
    let mut tool_meta: HashMap<String, (String, Map<String, Value>)> = HashMap::new();
    let mut file_reads: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut file_writes: HashMap<String, Vec<String>> = HashMap::new();
    let mut read_events: Vec<(String, i64, i64, bool)> = Vec::new(); // (file, size, at, deduped)
    let mut edit_files_at: Vec<(i64, String)> = Vec::new();
    let mut assistant_idx: i64 = 0;
    let mut prev_ts: Option<f64> = None;
    let mut had_gap = false;

    for raw in text.lines() {
        let Ok(line) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let msg = line.get("message").cloned().unwrap_or(Value::Null);
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = msg.get("content");

        if let Some(ts) = parse_ts(&line) {
            if let Some(prev) = prev_ts {
                let gap = ts - prev;
                if gap > 3600.0 {
                    r.gaps_over_1h += 1;
                    r.gaps_over_5m += 1;
                    had_gap = true;
                } else if gap > 300.0 {
                    r.gaps_over_5m += 1;
                    had_gap = true;
                }
            }
            prev_ts = Some(ts);
        }

        if role == "assistant" {
            if let Some(Value::Array(blocks)) = content {
                assistant_idx += 1;
                for b in blocks {
                    if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                        continue;
                    }
                    let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let inp = match b.get("input") {
                        Some(Value::Object(m)) => m.clone(),
                        _ => Map::new(),
                    };
                    let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    tool_meta.insert(id.to_string(), (name.to_string(), inp.clone()));
                    let fp = file_path_of(&inp);
                    if MUTATING_TOOLS.contains(&name) && !fp.is_empty() {
                        edit_files_at.push((assistant_idx, fp.clone()));
                        if name == "Write" {
                            let content = match inp.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(other) => other.to_string(),
                                None => String::new(),
                            };
                            file_writes.entry(fp).or_default().push(content);
                        }
                    }
                }
            }
        }

        if role == "user" {
            if let Some(Value::Array(blocks)) = content {
                for b in blocks {
                    if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                        continue;
                    }
                    let tid = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                    let (name, inp) = tool_meta
                        .get(tid)
                        .cloned()
                        .unwrap_or_else(|| (String::new(), Map::new()));
                    let text = block_text(b.get("content"));
                    let size = text.len() as i64;
                    bump(
                        &mut agg.tool_bytes,
                        if name.is_empty() { "unknown" } else { &name },
                        size,
                    );
                    if name != "Read" {
                        continue;
                    }

                    r.read_calls += 1;
                    r.read_bytes += size;
                    let fp = file_path_of(&inp);
                    let is_partial = inp.get("offset").map(|v| !v.is_null()).unwrap_or(false)
                        || inp.get("limit").map(|v| !v.is_null()).unwrap_or(false);
                    if size < MIN_SIZE {
                        r.read_calls_small += 1;
                    }
                    bump(&mut agg.class_bytes, classify_path(&fp), size);
                    r.linenum_overhead_bytes += linenum_re()
                        .find_iter(&text)
                        .map(|m| m.as_str().chars().count() as i64)
                        .sum::<i64>();

                    let h = hex::encode(Sha256::digest(text.as_bytes()));
                    let mut deduped = false;
                    if size >= MIN_SIZE && !fp.is_empty() {
                        let prior = file_reads.entry(fp.clone()).or_default();
                        if prior.iter().any(|(ph, _)| *ph == h) {
                            r.dedup_identical_bytes += size;
                            r.dedup_identical_calls += 1;
                            deduped = true;
                        } else if is_partial
                            && !text.is_empty()
                            && prior
                                .iter()
                                .any(|(_, pc)| pc.len() > text.len() && pc.contains(&text))
                        {
                            r.subset_bytes += size;
                            r.subset_calls += 1;
                        } else if file_writes.get(&fp).is_some_and(|writes| {
                            let trimmed = text.trim();
                            !trimmed.is_empty()
                                && writes
                                    .iter()
                                    .any(|w| !w.trim().is_empty() && w.contains(trimmed))
                        }) {
                            r.write_readback_bytes += size;
                            r.write_readback_calls += 1;
                        }
                    }
                    if !fp.is_empty() {
                        file_reads.entry(fp.clone()).or_default().push((h, text));
                    }
                    read_events.push((fp, size, assistant_idx, deduped));
                }
            }
        }
    }

    for (fp, size, at, deduped) in &read_events {
        if *size >= MIN_SIZE && !fp.is_empty() && !deduped {
            if edit_files_at.iter().any(|(idx, ef)| idx > at && ef == fp) {
                r.stale_bytes += size;
                r.stale_calls += 1;
            }
        }
        agg.residency.push((assistant_idx - at).max(0));
    }

    let mut per_file: HashMap<&str, i64> = HashMap::new();
    for (fp, _, _, _) in &read_events {
        if !fp.is_empty() {
            *per_file.entry(fp).or_default() += 1;
        }
    }
    if let Some(max) = per_file.values().max() {
        agg.reads_per_file_max.push(*max);
    }
    if had_gap {
        r.sessions_with_gap += 1;
    }
    r.sessions += 1;
    Ok(())
}

/// Audit all `*.jsonl` transcripts under `root`.
pub fn audit_reads(root: &Path) -> ReadAuditReport {
    let mut agg = ReadsAgg::default();
    for path in jsonl_files(root) {
        if audit_session(&path, &mut agg).is_err() {
            agg.report.files_skipped += 1;
        }
    }

    let mut r = agg.report;
    r.tool_bytes = agg.tool_bytes;
    r.tool_bytes.sort_by_key(|(_, b)| -b);
    r.class_bytes = agg.class_bytes;
    r.class_bytes.sort_by_key(|(_, b)| -b);
    if !agg.residency.is_empty() {
        let mut rt = agg.residency;
        rt.sort();
        r.residency_median = rt[rt.len() / 2];
        r.residency_p90 = pct_index(&rt, 0.9);
        r.residency_mean = rt.iter().sum::<i64>() as f64 / rt.len() as f64;
    }
    if !agg.reads_per_file_max.is_empty() {
        let mut m = agg.reads_per_file_max;
        m.sort();
        r.reads_per_file_max_median = m[m.len() / 2];
        r.reads_per_file_max = m[m.len() - 1];
    }
    r
}

fn fmt_bytes(b: i64) -> String {
    if b > 1_000_000 {
        format!("{:.1}MB (~{}K tok)", b as f64 / 1e6, b / 4000)
    } else {
        format!("{:.0}KB (~{}K tok)", b as f64 / 1000.0, b / 4000)
    }
}

/// Render the report as the human-readable summary.
pub fn render_text(r: &ReadAuditReport) -> String {
    let total_tool = r.tool_bytes.iter().map(|(_, b)| b).sum::<i64>().max(1);
    let rb = r.read_bytes.max(1);
    let mut out: Vec<String> = Vec::new();
    out.push(format!("sessions analyzed: {}", r.sessions));
    if r.files_skipped > 0 {
        out.push(format!("files skipped (unreadable): {}", r.files_skipped));
    }
    out.push("\n── tool_result bytes by tool ──".to_string());
    for (name, b) in r.tool_bytes.iter().take(10) {
        let label = if name.is_empty() { "?" } else { name };
        out.push(format!(
            "  {:<24} {:<28} {:.1}%",
            label,
            fmt_bytes(*b),
            100.0 * *b as f64 / total_tool as f64
        ));
    }
    out.push("\n── Read opportunity sizing (share of Read bytes) ──".to_string());
    out.push(format!(
        "  Read calls: {}  ({} below {}B floor)",
        r.read_calls, r.read_calls_small, MIN_SIZE
    ));
    out.push(format!(
        "  Read bytes: {}  = {:.1}% of all tool bytes",
        fmt_bytes(r.read_bytes),
        100.0 * r.read_bytes as f64 / total_tool as f64
    ));
    let rows = [
        ("identical repeat", r.dedup_identical_calls, r.dedup_identical_bytes),
        ("subset containment", r.subset_calls, r.subset_bytes),
        ("write-readback", r.write_readback_calls, r.write_readback_bytes),
        ("stale (edit after read)", r.stale_calls, r.stale_bytes),
    ];
    for (label, calls, b) in rows {
        out.push(format!(
            "  {:<32} {:>5} calls  {:<28} {:.1}%",
            label,
            calls,
            fmt_bytes(b),
            100.0 * b as f64 / rb as f64
        ));
    }
    out.push(format!(
        "  {:<32} {:>11}  {:<28} {:.1}%",
        "line-number scaffolding",
        "",
        fmt_bytes(r.linenum_overhead_bytes),
        100.0 * r.linenum_overhead_bytes as f64 / rb as f64
    ));
    out.push("\n── Read bytes by file class ──".to_string());
    for (cls, b) in &r.class_bytes {
        out.push(format!(
            "  {:<24} {:<28} {:.1}%",
            cls,
            fmt_bytes(*b),
            100.0 * *b as f64 / rb as f64
        ));
    }
    out.push("\n── context residency (assistant turns after each Read) ──".to_string());
    out.push(format!(
        "  median {}, p90 {}, mean {:.0}",
        r.residency_median, r.residency_p90, r.residency_mean
    ));
    out.push("\n── cache-death windows ──".to_string());
    out.push(format!(
        "  gaps >5min: {} ({} of them >1h); sessions with ≥1 gap: {}/{}",
        r.gaps_over_5m, r.gaps_over_1h, r.sessions_with_gap, r.sessions
    ));
    out.push(format!(
        "  max reads of one file per session: median {}, max {}",
        r.reads_per_file_max_median, r.reads_per_file_max
    ));
    out.join("\n")
}

// ─── maturation simulation (Mechanism B) ─────────────────────────────────

/// Aggregated simulation results.
#[derive(Default)]
pub struct MaturationSimReport {
    pub read_calls: i64,
    pub rereads_any: i64,
    pub rereads_partial: i64,
    pub big_reads: i64,
    pub big_read_bytes: i64,
    pub never_touched_again: i64,
    pub next_touch_p50: i64,
    pub next_touch_p90: i64,
    pub next_touch_p95: i64,
    /// quiesce N -> % of touched-again reads whose next touch is within N
    pub next_touch_within: Vec<(i64, f64)>,
    pub edits_with_prior_read: i64,
    pub edits_without_prior_read: i64,
    /// quiesce N -> edits whose file was quiet > N turns when edited
    pub at_risk_edits: Vec<(i64, i64)>,
}

impl MaturationSimReport {
    pub fn to_json_value(&self) -> Value {
        json!({
            "read_calls": self.read_calls,
            "rereads_any": self.rereads_any,
            "rereads_partial": self.rereads_partial,
            "big_reads": self.big_reads,
            "big_read_bytes": self.big_read_bytes,
            "never_touched_again": self.never_touched_again,
            "next_touch_p50": self.next_touch_p50,
            "next_touch_p90": self.next_touch_p90,
            "next_touch_p95": self.next_touch_p95,
            "next_touch_within": Value::Object(
                self.next_touch_within.iter().map(|(n, v)| (n.to_string(), json!(v))).collect()
            ),
            "edits_with_prior_read": self.edits_with_prior_read,
            "edits_without_prior_read": self.edits_without_prior_read,
            "at_risk_edits": Value::Object(
                self.at_risk_edits.iter().map(|(n, v)| (n.to_string(), json!(v))).collect()
            ),
        })
    }
}

/// Run the maturation simulation over `root/**/*.jsonl`.
pub fn simulate_maturation(root: &Path) -> MaturationSimReport {
    let mut r = MaturationSimReport::default();
    let mut next_touch_gaps: Vec<i64> = Vec::new();
    let mut at_risk: Vec<(i64, i64)> = QUIESCE_CANDIDATES.iter().map(|n| (*n, 0)).collect();

    for path in jsonl_files(root) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut tool_meta: HashMap<String, (String, Map<String, Value>)> = HashMap::new();
        // file -> [(turn, kind, size)]; kind: "edit" | "read"
        let mut timeline: HashMap<String, Vec<(i64, &'static str, i64)>> = HashMap::new();
        let mut session_reads: Vec<(String, i64, i64)> = Vec::new();
        let mut seen_files: HashSet<String> = HashSet::new();
        let mut a_idx: i64 = 0;

        for raw in text.lines() {
            let Ok(line) = serde_json::from_str::<Value>(raw) else {
                continue;
            };
            let msg = line.get("message").cloned().unwrap_or(Value::Null);
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let content = msg.get("content");
            if role == "assistant" {
                if let Some(Value::Array(blocks)) = content {
                    a_idx += 1;
                    for b in blocks {
                        if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                            continue;
                        }
                        let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let inp = match b.get("input") {
                            Some(Value::Object(m)) => m.clone(),
                            _ => Map::new(),
                        };
                        let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        tool_meta.insert(id.to_string(), (name.to_string(), inp.clone()));
                        let fp = file_path_of(&inp);
                        if MUTATING_TOOLS.contains(&name) && !fp.is_empty() {
                            timeline.entry(fp).or_default().push((a_idx, "edit", 0));
                        }
                    }
                }
            }
            if role == "user" {
                if let Some(Value::Array(blocks)) = content {
                    for b in blocks {
                        if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                            continue;
                        }
                        let tid = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                        let (name, inp) = tool_meta
                            .get(tid)
                            .cloned()
                            .unwrap_or_else(|| (String::new(), Map::new()));
                        if name != "Read" {
                            continue;
                        }
                        let fp = file_path_of(&inp);
                        if fp.is_empty() {
                            continue;
                        }
                        let text = block_text(b.get("content"));
                        let size = text.len() as i64;
                        let partial = inp.get("offset").map(|v| !v.is_null()).unwrap_or(false)
                            || inp.get("limit").map(|v| !v.is_null()).unwrap_or(false);
                        r.read_calls += 1;
                        if seen_files.contains(&fp) {
                            r.rereads_any += 1;
                            if partial {
                                r.rereads_partial += 1;
                            }
                        }
                        seen_files.insert(fp.clone());
                        timeline.entry(fp.clone()).or_default().push((a_idx, "read", size));
                        session_reads.push((fp, a_idx, size));
                    }
                }
            }
        }

        for ops in timeline.values_mut() {
            ops.sort_by_key(|(turn, _, _)| *turn);
            let snapshot = ops.clone();
            for (turn, kind, _size) in &snapshot {
                if *kind != "edit" {
                    continue;
                }
                let prev: Vec<i64> = snapshot
                    .iter()
                    .filter(|(t, _, _)| t < turn)
                    .map(|(t, _, _)| *t)
                    .collect();
                let had_read = snapshot
                    .iter()
                    .any(|(t, k, _)| *k == "read" && t <= turn);
                if !had_read {
                    r.edits_without_prior_read += 1;
                    continue;
                }
                r.edits_with_prior_read += 1;
                if let Some(latest) = prev.iter().max() {
                    let gap = turn - latest;
                    for (n, count) in at_risk.iter_mut() {
                        if gap > *n {
                            *count += 1;
                        }
                    }
                }
            }
        }

        for (fp, rturn, size) in &session_reads {
            if *size < MATURE_FLOOR {
                continue;
            }
            r.big_reads += 1;
            r.big_read_bytes += size;
            let later: Option<i64> = timeline
                .get(fp)
                .into_iter()
                .flatten()
                .filter(|(t, _, _)| t > rturn)
                .map(|(t, _, _)| *t)
                .min();
            match later {
                Some(next) => next_touch_gaps.push(next - rturn),
                None => r.never_touched_again += 1,
            }
        }
    }

    next_touch_gaps.sort();
    r.next_touch_p50 = pct_index(&next_touch_gaps, 0.5);
    r.next_touch_p90 = pct_index(&next_touch_gaps, 0.9);
    r.next_touch_p95 = pct_index(&next_touch_gaps, 0.95);
    if !next_touch_gaps.is_empty() {
        let total = next_touch_gaps.len() as f64;
        r.next_touch_within = QUIESCE_CANDIDATES
            .iter()
            .map(|n| {
                let within = next_touch_gaps.iter().filter(|g| *g <= n).count() as f64;
                let pct = 100.0 * within / total;
                (*n, (pct * 10.0).round_ties_even() / 10.0)
            })
            .collect();
    }
    r.at_risk_edits = at_risk;
    r
}

/// Human-readable simulation summary.
pub fn render_sim_text(r: &MaturationSimReport) -> String {
    let mut out: Vec<String> = Vec::new();
    out.push("── maturation simulation (Mechanism B) ──".to_string());
    out.push(format!(
        "  re-reads: {}/{} reads target an already-read file ({:.1}%); {} of those are partial ranges",
        r.rereads_any,
        r.read_calls,
        100.0 * r.rereads_any as f64 / r.read_calls.max(1) as f64,
        r.rereads_partial
    ));
    out.push(format!(
        "  big reads (≥{}B): {} ({:.1}MB); never touched again: {} ({:.1}%) ← pure savings",
        MATURE_FLOOR,
        r.big_reads,
        r.big_read_bytes as f64 / 1e6,
        r.never_touched_again,
        100.0 * r.never_touched_again as f64 / r.big_reads.max(1) as f64
    ));
    out.push(format!(
        "  next-touch gap for the rest (turns): p50={} p90={} p95={}",
        r.next_touch_p50, r.next_touch_p90, r.next_touch_p95
    ));
    for (n, share) in &r.next_touch_within {
        out.push(format!("    next touch within {n:>2} turn(s): {share:.1}%"));
    }
    let total_edits = r.edits_with_prior_read + r.edits_without_prior_read;
    out.push(format!(
        "  edits: {} with a prior read of the file, {} without",
        r.edits_with_prior_read, r.edits_without_prior_read
    ));
    out.push("  activity-based at-risk edits (file quiet > N turns when edited):".to_string());
    for (n, count) in &r.at_risk_edits {
        out.push(format!(
            "    quiesce {:>2}: {:>5} edits ({:.1}%)",
            n,
            count,
            100.0 * *count as f64 / total_edits.max(1) as f64
        ));
    }
    out.join("\n")
}

// ─── codex audit (shell-based reads) ─────────────────────────────────────

const READ_PROGS: &[&str] = &["cat", "sed", "head", "tail", "nl", "bat", "more", "read"];
const SEARCH_PROGS: &[&str] = &["rg", "grep", "ugrep", "ag", "fd", "find"];
const BUILD_PROGS: &[&str] =
    &["python", "python3", "pytest", "cargo", "npm", "make", "uv", "ruff"];

fn range_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d+([,:-]\d+)?p?$").unwrap())
}

/// Aggregated Codex read-pattern results.
#[derive(Default)]
pub struct CodexAuditReport {
    pub sessions: i64,
    pub exec_calls: i64,
    pub calls_by_category: Vec<(String, i64)>,
    pub bytes_by_category: Vec<(String, i64)>,
    pub total_output_bytes: i64,
    pub read_calls: i64,
    pub read_bytes: i64,
    pub reads_partial: i64,
    pub rereads_same_path: i64,
    pub distinct_files_read: i64,
    pub reads_over_floor: i64,
    pub read_size_p50: i64,
    pub read_size_p90: i64,
    pub top_reread_files: Vec<(String, i64)>,
}

impl CodexAuditReport {
    pub fn to_json_value(&self) -> Value {
        json!({
            "sessions": self.sessions,
            "exec_calls": self.exec_calls,
            "calls_by_category": pairs_to_object(&self.calls_by_category),
            "bytes_by_category": pairs_to_object(&self.bytes_by_category),
            "total_output_bytes": self.total_output_bytes,
            "read_calls": self.read_calls,
            "read_bytes": self.read_bytes,
            "reads_partial": self.reads_partial,
            "rereads_same_path": self.rereads_same_path,
            "distinct_files_read": self.distinct_files_read,
            "reads_over_floor": self.reads_over_floor,
            "read_size_p50": self.read_size_p50,
            "read_size_p90": self.read_size_p90,
            "top_reread_files": self.top_reread_files
                .iter()
                .map(|(f, n)| json!([f, n]))
                .collect::<Vec<_>>(),
        })
    }
}

/// Peel rtk wrappers: `rtk <cmd>` and `rtk proxy <cmd>`.
pub fn strip_wrappers(cmd: &str) -> &str {
    let mut c = cmd.trim();
    loop {
        if let Some(rest) = c.strip_prefix("rtk ") {
            c = rest.trim();
            continue;
        }
        if let Some(rest) = c.strip_prefix("proxy ") {
            c = rest.trim();
            continue;
        }
        return c;
    }
}

/// Minimal POSIX-ish `shlex.split`; `None` mirrors Python's ValueError
/// (unbalanced quote), where the caller falls back to whitespace split.
fn shlex_split(s: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            '\'' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => current.push(ch),
                        None => return None, // no closing quotation
                    }
                }
            }
            '"' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('"' | '\\')) => current.push(ch),
                            Some(ch) => {
                                current.push('\\');
                                current.push(ch);
                            }
                            None => return None,
                        },
                        Some(ch) => current.push(ch),
                        None => return None, // no closing quotation
                    }
                }
            }
            '\\' => {
                in_token = true;
                if let Some(ch) = chars.next() {
                    current.push(ch);
                }
            }
            _ => {
                in_token = true;
                current.push(c);
            }
        }
    }
    if in_token {
        tokens.push(current);
    }
    Some(tokens)
}

/// Classify a shell command: (category, file_path, is_partial).
///
/// Categories: read, search, git, edit, build/test, compound, other.
/// For reads, the path is resolved against `workdir` when relative.
pub fn classify_command(cmd: &str, workdir: &str) -> (&'static str, Option<String>, bool) {
    let c = strip_wrappers(cmd);
    if c.contains("apply_patch") {
        return ("edit", None, false);
    }
    let toks = shlex_split(c)
        .unwrap_or_else(|| c.split_whitespace().map(str::to_string).collect());
    if toks.is_empty() {
        return ("other", None, false);
    }
    let prog = toks[0].rsplit('/').next().unwrap_or("");

    if READ_PROGS.contains(&prog) {
        let is_rangey = |t: &str| range_re().is_match(t.trim_matches(|ch| ch == '\'' || ch == '"'));
        let candidates: Vec<&String> = toks[1..]
            .iter()
            .filter(|t| {
                !t.starts_with('-')
                    && (t.contains('/') || t.rsplit('/').next().unwrap_or("").contains('.'))
            })
            // Range tokens like 1,200p (sed) are not paths.
            .filter(|t| !is_rangey(t))
            .collect();
        let mut fpath = candidates.first().map(|t| (*t).clone());
        if let Some(p) = &fpath {
            if !workdir.is_empty() && !p.starts_with('/') {
                fpath = Some(format!("{}/{p}", workdir.trim_end_matches('/')));
            }
        }
        let partial = matches!(prog, "sed" | "head" | "tail")
            || toks[1..].iter().any(|t| is_rangey(t))
            || c.contains("--lines");
        return ("read", fpath, partial);
    }
    if SEARCH_PROGS.contains(&prog) {
        return ("search", None, false);
    }
    if prog == "git" {
        return ("git", None, false);
    }
    if BUILD_PROGS.contains(&prog) {
        return ("build/test", None, false);
    }
    if cmd.contains("&&") || cmd.contains('|') {
        for part in cmd.split(|ch| ch == '|').flat_map(|s| s.split("&&")) {
            let (cat, fpath, partial) = classify_command(part, workdir);
            if cat == "read" {
                return (cat, fpath, partial);
            }
        }
        return ("compound", None, false);
    }
    ("other", None, false)
}

fn output_text(payload: &Value) -> String {
    match payload.get("output") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(map)) => match map.get("output") {
            Some(inner) if truthy(inner) => match inner {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            },
            _ => Value::Object(map.clone()).to_string(),
        },
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Audit all Codex `*.jsonl` transcripts under `root`.
pub fn audit_codex(root: &Path) -> CodexAuditReport {
    let mut r = CodexAuditReport::default();
    let mut calls: Vec<(String, i64)> = Vec::new();
    let mut cat_bytes: Vec<(String, i64)> = Vec::new();
    let mut read_sizes: Vec<i64> = Vec::new();
    let mut per_file_reads: Vec<(String, i64)> = Vec::new();

    for path in jsonl_files(root) {
        let mut pending: HashMap<String, &'static str> = HashMap::new();
        let mut seen_paths: HashSet<String> = HashSet::new();
        let mut saw_lines = false;
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        for raw in text.lines() {
            let Ok(line) = serde_json::from_str::<Value>(raw) else {
                continue;
            };
            saw_lines = true;
            let pl = line.get("payload").cloned().unwrap_or(Value::Null);
            let t = pl.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if t == "function_call" && pl.get("name").and_then(|v| v.as_str()) == Some("exec_command")
            {
                let args: Map<String, Value> = pl
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .and_then(|v| match v {
                        Value::Object(m) => Some(m),
                        _ => None,
                    })
                    .unwrap_or_default();
                let (cat, fpath, partial) =
                    classify_command(get_str(&args, "cmd"), get_str(&args, "workdir"));
                r.exec_calls += 1;
                bump(&mut calls, cat, 1);
                let call_id = pl.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                pending.insert(call_id.to_string(), cat);
                if cat == "read" {
                    r.read_calls += 1;
                    r.reads_partial += i64::from(partial);
                    if let Some(fpath) = fpath {
                        if seen_paths.contains(&fpath) {
                            r.rereads_same_path += 1;
                        }
                        seen_paths.insert(fpath.clone());
                        bump(&mut per_file_reads, &fpath, 1);
                    }
                }
            } else if t == "function_call_output" {
                let size = output_text(&pl).len() as i64;
                r.total_output_bytes += size;
                let call_id = pl.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                let cat = pending.get(call_id).copied().unwrap_or("untracked");
                bump(&mut cat_bytes, cat, size);
                if cat == "read" {
                    r.read_bytes += size;
                    read_sizes.push(size);
                    if size >= MATURE_FLOOR {
                        r.reads_over_floor += 1;
                    }
                }
            }
        }
        if saw_lines {
            r.sessions += 1;
        }
    }

    calls.sort_by_key(|(_, n)| -n);
    r.calls_by_category = calls;
    cat_bytes.sort_by_key(|(_, b)| -b);
    r.bytes_by_category = cat_bytes;
    r.distinct_files_read = per_file_reads.len() as i64;
    if !read_sizes.is_empty() {
        read_sizes.sort();
        r.read_size_p50 = read_sizes[read_sizes.len() / 2];
        r.read_size_p90 = pct_index(&read_sizes, 0.9);
    }
    per_file_reads.sort_by_key(|(_, n)| -n);
    r.top_reread_files = per_file_reads
        .iter()
        .take(5)
        .filter(|(_, n)| *n > 1)
        .map(|(f, n)| (f.rsplit('/').next().unwrap_or("").to_string(), *n))
        .collect();
    r
}

/// Human-readable Codex audit summary.
pub fn render_codex_text(r: &CodexAuditReport) -> String {
    let total = r.total_output_bytes.max(1);
    let mut out: Vec<String> = Vec::new();
    out.push("── codex read-pattern audit ──".to_string());
    out.push(format!(
        "  sessions: {}, exec_command calls: {}",
        r.sessions, r.exec_calls
    ));
    out.push(format!(
        "  output bytes by category ({:.1}MB total):",
        total as f64 / 1e6
    ));
    for (cat, b) in &r.bytes_by_category {
        out.push(format!(
            "    {:<12} {:>6.2}MB  {:.1}%",
            cat,
            *b as f64 / 1e6,
            100.0 * *b as f64 / total as f64
        ));
    }
    let rc = r.read_calls.max(1);
    out.push(format!(
        "  reads: {} ({:.0}% partial slices); re-reads of same path: {} ({:.0}%)",
        r.read_calls,
        100.0 * r.reads_partial as f64 / rc as f64,
        r.rereads_same_path,
        100.0 * r.rereads_same_path as f64 / rc as f64
    ));
    out.push(format!(
        "  distinct files read: {}; read size p50={}B p90={}B; ≥{}B: {} ({:.0}%)",
        r.distinct_files_read,
        r.read_size_p50,
        r.read_size_p90,
        MATURE_FLOOR,
        r.reads_over_floor,
        100.0 * r.reads_over_floor as f64 / rc as f64
    ));
    if !r.top_reread_files.is_empty() {
        // Python renders the list of tuples with repr: [('foo.py', 3), ...].
        let items: Vec<String> = r
            .top_reread_files
            .iter()
            .map(|(f, n)| format!("('{f}', {n})"))
            .collect();
        out.push(format!("  most re-read files: [{}]", items.join(", ")));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: &str = "2026-06-09T10:00:00Z";

    fn line(role: &str, content: Value, ts: &str) -> String {
        json!({"message": {"role": role, "content": content}, "timestamp": ts}).to_string()
    }

    fn tool_use(id: &str, name: &str, inp: Value) -> Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": inp})
    }

    fn tool_result(id: &str, text: &str) -> Value {
        json!({"type": "tool_result", "tool_use_id": id, "content": text})
    }

    fn content() -> String {
        "     1\tdef foo():\n     2\t    return 42\n".repeat(30) // >512B
    }

    /// Synthetic session: read foo.py twice (identical), partial read
    /// contained in the full read, edit foo.py, then a >5min gap.
    fn transcript_dir(root: &Path) -> PathBuf {
        let c = content();
        let half: String = c.chars().take(c.chars().count() / 2).collect();
        let lines = vec![
            line("user", json!("look at foo.py"), "2026-06-09T10:00:00Z"),
            line(
                "assistant",
                json!([tool_use("r1", "Read", json!({"file_path": "/x/foo.py"}))]),
                "2026-06-09T10:00:01Z",
            ),
            line("user", json!([tool_result("r1", &c)]), "2026-06-09T10:00:02Z"),
            line(
                "assistant",
                json!([tool_use("r2", "Read", json!({"file_path": "/x/foo.py"}))]),
                "2026-06-09T10:00:03Z",
            ),
            line("user", json!([tool_result("r2", &c)]), "2026-06-09T10:00:04Z"),
            line(
                "assistant",
                json!([tool_use(
                    "r3",
                    "Read",
                    json!({"file_path": "/x/foo.py", "offset": 1, "limit": 2})
                )]),
                "2026-06-09T10:00:05Z",
            ),
            line("user", json!([tool_result("r3", &half)]), "2026-06-09T10:00:06Z"),
            line(
                "assistant",
                json!([tool_use(
                    "e1",
                    "Edit",
                    json!({"file_path": "/x/foo.py", "old_string": "a"})
                )]),
                "2026-06-09T10:00:07Z",
            ),
            line("user", json!([tool_result("e1", "ok")]), "2026-06-09T10:00:08Z"),
            line("user", json!("back from lunch"), "2026-06-09T10:20:00Z"),
        ];
        let proj = root.join("projects").join("-x-demo");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("session1.jsonl"), lines.join("\n")).unwrap();
        root.join("projects")
    }

    #[test]
    fn audit_reads_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let root = transcript_dir(dir.path());
        let r = audit_reads(&root);
        assert_eq!(r.sessions, 1);
        assert_eq!(r.read_calls, 3);
        assert_eq!(r.dedup_identical_calls, 1); // r2 == r1
        assert_eq!(r.subset_calls, 1); // r3 ⊂ r1
        assert_eq!(r.stale_calls, 2);
        assert_eq!(r.gaps_over_5m, 1);
        assert_eq!(r.sessions_with_gap, 1);
        assert!(r.linenum_overhead_bytes > 0);
        assert!(r
            .class_bytes
            .iter()
            .any(|(c, b)| c == "source code" && *b > 0));
        let read_tool_bytes = r
            .tool_bytes
            .iter()
            .find(|(n, _)| n == "Read")
            .map(|(_, b)| *b)
            .unwrap();
        assert_eq!(read_tool_bytes, r.read_bytes);
        assert_eq!(r.reads_per_file_max, 3);
    }

    #[test]
    fn render_text_runs() {
        let dir = tempfile::tempdir().unwrap();
        let root = transcript_dir(dir.path());
        let out = render_text(&audit_reads(&root));
        assert!(out.contains("Read opportunity sizing"));
        assert!(out.contains("identical repeat"));
        assert!(out.contains("cache-death windows"));
    }

    #[test]
    fn malformed_lines_tolerated_and_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("bad.jsonl"),
            format!("not json\n{{\n{}", line("user", json!("hi"), TS)),
        )
        .unwrap();
        let r = audit_reads(dir.path());
        assert_eq!(r.sessions, 1);
        assert_eq!(r.read_calls, 0);

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(audit_reads(empty.path()).sessions, 0);
    }

    #[test]
    fn maturation_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let root = transcript_dir(dir.path());
        let r = simulate_maturation(&root);
        assert_eq!(r.read_calls, 3);
        assert_eq!(r.rereads_any, 2);
        assert_eq!(r.rereads_partial, 1);
        assert_eq!(r.big_reads, 0); // CONTENT ~1.2KB < 2KB floor
        assert_eq!(r.edits_with_prior_read, 1);
        assert_eq!(r.at_risk_edits[0], (1, 0));
    }

    #[test]
    fn maturation_big_read_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let big = "x".repeat((MATURE_FLOOR + 100) as usize);
        let lines = vec![
            line(
                "assistant",
                json!([tool_use("r1", "Read", json!({"file_path": "/x/big.py"}))]),
                TS,
            ),
            line("user", json!([tool_result("r1", &big)]), TS),
        ];
        std::fs::write(proj.join("s.jsonl"), lines.join("\n")).unwrap();
        let r = simulate_maturation(dir.path());
        assert_eq!(r.big_reads, 1);
        assert_eq!(r.never_touched_again, 1);
    }

    #[test]
    fn maturation_render_runs() {
        let dir = tempfile::tempdir().unwrap();
        let root = transcript_dir(dir.path());
        let out = render_sim_text(&simulate_maturation(&root));
        assert!(out.contains("maturation simulation"));
        assert!(out.contains("at-risk edits"));
    }

    #[test]
    fn strip_wrappers_cases() {
        assert_eq!(strip_wrappers("rtk cat foo.py"), "cat foo.py");
        assert_eq!(
            strip_wrappers("rtk proxy sed -n '1,20p' foo.py"),
            "sed -n '1,20p' foo.py"
        );
        assert_eq!(strip_wrappers("git status"), "git status");
    }

    #[test]
    fn classify_categories() {
        let cases: &[(&str, &str, bool)] = &[
            ("cat src/foo.py", "read", false),
            ("sed -n '1,200p' src/foo.py", "read", true),
            ("rtk read src/foo.py --lines 10-50", "read", true),
            ("head -50 src/foo.py", "read", true),
            ("nl headroom/config.py", "read", false),
            ("rg -n 'def apply' headroom/", "search", false),
            ("rtk grep -n pattern .", "search", false),
            ("git diff HEAD~1", "git", false),
            ("apply_patch <<'EOF'\n*** Begin Patch\nEOF", "edit", false),
            ("pytest tests/ -x -q", "build/test", false),
            ("echo hello", "other", false),
        ];
        for (cmd, category, partial) in cases {
            let (cat, _path, is_partial) = classify_command(cmd, "");
            assert_eq!(cat, *category, "cmd: {cmd}");
            if *category == "read" {
                assert_eq!(is_partial, *partial, "cmd: {cmd}");
            }
        }
    }

    #[test]
    fn classify_path_extraction_and_workdir() {
        let (_, path, _) = classify_command("cat src/foo.py", "/repo");
        assert_eq!(path.as_deref(), Some("/repo/src/foo.py"));
        let (_, path, _) = classify_command("cat /abs/foo.py", "/repo");
        assert_eq!(path.as_deref(), Some("/abs/foo.py"));
        let (_, path, _) = classify_command("sed -n '5,30p' headroom/config.py", "");
        assert_eq!(path.as_deref(), Some("headroom/config.py"));
        let (cat, path, _) = classify_command("cat foo.py | grep def", "/r");
        assert_eq!(cat, "read");
        assert_eq!(path.as_deref(), Some("/r/foo.py"));
    }

    fn codex_call(call_id: &str, cmd: &str) -> String {
        json!({"payload": {
            "type": "function_call",
            "name": "exec_command",
            "call_id": call_id,
            "arguments": json!({"cmd": cmd, "workdir": "/repo"}).to_string(),
        }})
        .to_string()
    }

    fn codex_output(call_id: &str, text: &str) -> String {
        json!({"payload": {
            "type": "function_call_output",
            "call_id": call_id,
            "output": text,
        }})
        .to_string()
    }

    fn codex_dir(root: &Path) -> PathBuf {
        let content = "line\n".repeat(600); // 3000B — over the maturation floor
        let lines = vec![
            codex_call("c1", "cat src/foo.py"),
            codex_output("c1", &content),
            codex_call("c2", "sed -n '1,100p' src/foo.py"),
            codex_output("c2", &content[..500]),
            codex_call("c3", "rg -n 'def ' src/"),
            codex_output("c3", "src/foo.py:1:def x():"),
            codex_call("c4", "rtk read src/bar.py --lines 1-50"),
            codex_output("c4", &"bar content ".repeat(10)),
        ];
        let sessions = root.join("sessions").join("2026").join("06");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("rollout-1.jsonl"), lines.join("\n")).unwrap();
        root.join("sessions")
    }

    #[test]
    fn audit_codex_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let root = codex_dir(dir.path());
        let r = audit_codex(&root);
        assert_eq!(r.sessions, 1);
        assert_eq!(r.exec_calls, 4);
        assert_eq!(r.read_calls, 3); // c1, c2, c4
        assert_eq!(r.reads_partial, 2); // c2 (sed range), c4 (--lines)
        assert_eq!(r.rereads_same_path, 1); // c2 re-reads foo.py
        assert_eq!(r.distinct_files_read, 2);
        assert_eq!(r.reads_over_floor, 1); // c1 (3000B)
        let count = |pairs: &[(String, i64)], k: &str| {
            pairs.iter().find(|(c, _)| c == k).map(|(_, n)| *n).unwrap_or(0)
        };
        assert_eq!(count(&r.calls_by_category, "search"), 1);
        assert!(count(&r.bytes_by_category, "read") > count(&r.bytes_by_category, "search"));
        let out = render_codex_text(&r);
        assert!(out.contains("codex read-pattern audit"));
        assert!(out.contains("partial slices"));

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(audit_codex(empty.path()).sessions, 0);
    }

    #[test]
    fn py_dumps_sorted_orders_numeric_keys() {
        let value = json!({"b": 1, "a": {"10": 1, "2": 2, "1": 3}});
        let dumped = py_dumps_sorted(&value);
        let pos = |s: &str| dumped.find(s).unwrap();
        assert!(pos("\"a\"") < pos("\"b\""));
        assert!(pos("\"1\"") < pos("\"2\""));
        assert!(pos("\"2\"") < pos("\"10\""));
    }
}
