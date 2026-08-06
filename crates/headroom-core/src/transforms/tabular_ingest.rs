//! Tabular-text compressor: bridges CSV/TSV/markdown tables to SmartCrusher.
//!
//! Raw tabular text has no native compressor — it would otherwise fall through
//! to plain-text Kompress, ignoring its row/column structure. This module
//! parses tabular text into a JSON array of records. The actual compression
//! is delegated to SmartCrusher (already in Rust).
//!
//! No new compression algorithm and no new CCR plumbing live here — only the
//! text→records bridge.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::transforms::content_detector::{detect_content_type, ContentType};

// Mirrors content_detector's separator-cell pattern.
fn md_sep_cell_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^:?-{2,}:?$").unwrap())
}

fn fixed_width_splitter_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s{2,}").unwrap())
}

/// Configuration for tabular-text compression.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabularCompressorConfig {
    /// Pass-through to SmartCrusher's lossless renderer.
    pub compaction_format: String,
    /// Only keep SmartCrusher's output if it is strictly smaller.
    pub min_savings_chars: usize,
}

impl Default for TabularCompressorConfig {
    fn default() -> Self {
        Self {
            compaction_format: "csv-schema".to_string(),
            min_savings_chars: 1,
        }
    }
}

/// Result of tabular-text compression.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabularCompressionResult {
    pub compressed: String,
    pub original: String,
    pub was_modified: bool,
    pub fmt: String,
    pub rows: usize,
    pub columns: usize,
    #[serde(default = "default_strategy")]
    pub strategy: String,
}

fn default_strategy() -> String {
    "tabular".to_string()
}

impl TabularCompressionResult {
    pub fn compression_ratio(&self) -> f64 {
        if self.original.is_empty() {
            return 0.0;
        }
        self.compressed.len() as f64 / self.original.len() as f64
    }
}

// ─── Parsers (text → headers + rows) ─────────────────────────────────────

/// Parse delimited text (CSV/TSV).
pub fn parse_csv(content: &str, delimiter: char) -> (Vec<String>, Vec<Vec<String>>) {
    let mut parsed: Vec<Vec<String>> = Vec::new();
    for line in content.lines() {
        let row: Vec<String> = line
            .split(delimiter)
            .map(|c| c.trim().to_string())
            .collect();
        if row.iter().any(|c| !c.is_empty()) {
            parsed.push(row);
        }
    }
    if parsed.is_empty() {
        return (vec![], vec![]);
    }
    let headers = parsed.remove(0);
    (headers, parsed)
}

/// Parse a markdown table, dropping the `|---|` separator row.
pub fn parse_markdown_table(content: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let lines: Vec<&str> = content
        .lines()
        .filter(|ln| !ln.trim().is_empty() && ln.contains('|'))
        .collect();

    if lines.len() < 2 {
        return (vec![], vec![]);
    }

    fn split_row(row: &str) -> Vec<String> {
        row.trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect()
    }

    fn is_separator(row: &str) -> bool {
        let row_strs = split_row(row);
        let cells: Vec<&str> = row_strs
            .iter()
            .filter(|c| !c.is_empty())
            .map(|s| s.as_str())
            .collect();
        cells.len() >= 2 && cells.iter().all(|c| md_sep_cell_re().is_match(c))
    }

    let headers = split_row(lines[0]);
    let rows: Vec<Vec<String>> = lines[1..]
        .iter()
        .filter(|ln| !is_separator(ln))
        .map(|ln| split_row(ln))
        .collect();

    (headers, rows)
}

/// Parse whitespace-aligned columns (best-effort, ≥ 2 spaces as a gap).
pub fn parse_fixed_width(content: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let lines: Vec<&str> = content.lines().filter(|ln| !ln.trim().is_empty()).collect();
    if lines.len() < 2 {
        return (vec![], vec![]);
    }
    let re = fixed_width_splitter_re();
    let headers: Vec<String> = re.split(lines[0].trim()).map(|s| s.to_string()).collect();
    let rows: Vec<Vec<String>> = lines[1..]
        .iter()
        .map(|ln| re.split(ln.trim()).map(|s| s.to_string()).collect())
        .collect();
    (headers, rows)
}

/// Zip headers with each row into dicts, padding/truncating to width.
pub fn to_records(headers: &[String], rows: &[Vec<String>]) -> Vec<HashMap<String, String>> {
    if headers.is_empty() {
        return vec![];
    }
    let width = headers.len();
    rows.iter()
        .map(|row| {
            let padded: Vec<&str> = row
                .iter()
                .map(|s| s.as_str())
                .chain(std::iter::repeat(""))
                .take(width)
                .collect();
            headers
                .iter()
                .zip(padded.into_iter())
                .map(|(h, v)| (h.clone(), v.to_string()))
                .collect()
        })
        .collect()
}

/// Detect the tabular format and parse to (headers, rows, fmt).
/// Returns None if the content is not tabular.
pub fn parse_tabular(content: &str) -> Option<(Vec<String>, Vec<Vec<String>>, String)> {
    let detection = detect_content_type(content);
    if detection.content_type != ContentType::Tabular {
        return None;
    }

    let fmt = detection
        .metadata
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("csv");
    let (headers, rows) = match fmt {
        "markdown" => parse_markdown_table(content),
        "fixed_width" => parse_fixed_width(content),
        _ => {
            let delimiter = detection
                .metadata
                .get("delimiter")
                .and_then(|v| v.as_str())
                .and_then(|s| s.chars().next())
                .unwrap_or(',');
            parse_csv(content, delimiter)
        }
    };

    if headers.is_empty() || rows.is_empty() {
        return None;
    }
    // Ragged tables (rows whose cell count differs from the header count)
    // can't be zipped into records without shifting values under the wrong
    // column — a compressed table must never state facts the original
    // didn't (#1652). Treat them as non-tabular and pass through verbatim.
    let width = headers.len();
    if rows.iter().any(|row| row.len() != width) {
        return None;
    }
    Some((headers, rows, fmt.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "name,age,city\nAlice,30,NYC\nBob,25,LA\nCara,40,SF";
    const TSV: &str = "id\tval\tnote\n1\ta\tx\n2\tb\ty\n3\tc\tz";
    const MARKDOWN: &str =
        "| name | age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |\n| Cara | 40 |";

    // ── parse_csv ────────────────────────────────────────────────────────

    #[test]
    fn parse_csv_and_records() {
        let (headers, rows) = parse_csv(CSV, ',');
        assert_eq!(headers, vec!["name", "age", "city"]);
        assert_eq!(rows[0], vec!["Alice", "30", "NYC"]);
        let records = to_records(&headers, &rows);
        assert_eq!(records[1].get("name").unwrap(), "Bob");
        assert_eq!(records[1].get("age").unwrap(), "25");
        assert_eq!(records[1].get("city").unwrap(), "LA");
    }

    #[test]
    fn parse_csv_blank_returns_empty() {
        let (headers, rows) = parse_csv("   \n  \n", ',');
        assert!(headers.is_empty());
        assert!(rows.is_empty());
    }

    #[test]
    fn parse_tsv() {
        let (headers, rows) = parse_csv(TSV, '\t');
        assert_eq!(headers, vec!["id", "val", "note"]);
        assert_eq!(rows.len(), 3);
    }

    // ── parse_markdown_table ─────────────────────────────────────────────

    #[test]
    fn parse_markdown_table_drops_separator() {
        let (headers, rows) = parse_markdown_table(MARKDOWN);
        assert_eq!(headers, vec!["name", "age"]);
        assert!(rows.iter().any(|r| r[0] == "Alice" && r[1] == "30"));
        for row in &rows {
            for cell in row {
                assert!(!cell.contains("---"));
            }
        }
    }

    #[test]
    fn parse_markdown_table_too_short_returns_empty() {
        let (headers, rows) = parse_markdown_table("| only one row |");
        assert!(headers.is_empty());
        assert!(rows.is_empty());
    }

    // ── parse_fixed_width ────────────────────────────────────────────────

    #[test]
    fn parse_fixed_width_basic() {
        let (headers, rows) =
            parse_fixed_width("name    age   city\nAlice   30    NYC\nBob     25    LA");
        assert_eq!(headers, vec!["name", "age", "city"]);
        assert_eq!(rows[0], vec!["Alice", "30", "NYC"]);
    }

    #[test]
    fn parse_fixed_width_too_short_returns_empty() {
        let (headers, rows) = parse_fixed_width("a single line");
        assert!(headers.is_empty());
        assert!(rows.is_empty());
    }

    // ── to_records ───────────────────────────────────────────────────────

    #[test]
    fn to_records_empty_headers_returns_empty() {
        assert!(to_records(&[], &[vec!["a".to_string(), "b".to_string()]]).is_empty());
    }

    // ── parse_tabular ────────────────────────────────────────────────────

    #[test]
    fn parse_tabular_returns_none_for_non_tabular() {
        assert!(parse_tabular("just a normal paragraph here").is_none());
    }

    #[test]
    fn parse_tabular_csv() {
        let result = parse_tabular(CSV);
        assert!(result.is_some());
        let (headers, rows, fmt) = result.unwrap();
        assert_eq!(fmt, "csv");
        assert_eq!(headers, vec!["name", "age", "city"]);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn parse_tabular_markdown() {
        let result = parse_tabular(MARKDOWN);
        assert!(result.is_some());
        let (_, _, fmt) = result.unwrap();
        assert_eq!(fmt, "markdown");
    }

    #[test]
    fn parse_tabular_none_when_no_data_rows_survive() {
        // Header + separator only, no data rows
        assert!(parse_tabular("| a | b |\n| --- | --- |\n| --- | --- |").is_none());
    }

    // ── TabularCompressionResult ─────────────────────────────────────────

    #[test]
    fn compression_ratio_zero_for_empty_original() {
        let result = TabularCompressionResult {
            compressed: String::new(),
            original: String::new(),
            was_modified: false,
            fmt: "csv".to_string(),
            rows: 0,
            columns: 0,
            strategy: "tabular".to_string(),
        };
        assert_eq!(result.compression_ratio(), 0.0);
    }

    #[test]
    fn compression_ratio_normal() {
        let result = TabularCompressionResult {
            compressed: "abc".to_string(),
            original: "abcdef".to_string(),
            was_modified: true,
            fmt: "csv".to_string(),
            rows: 1,
            columns: 3,
            strategy: "tabular".to_string(),
        };
        assert!((result.compression_ratio() - 0.5).abs() < f64::EPSILON);
    }

    // ── verbose markdown ─────────────────────────────────────────────────

    fn verbose_markdown(rows: usize) -> String {
        let body: String = (0..rows)
            .map(|i| {
                format!(
                    "| user_{} | {} | city_{} | active | engineering |",
                    i,
                    20 + i,
                    i % 5
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "| name | age | city | status | dept |\n| --- | --- | --- | --- | --- |\n{}",
            body
        )
    }

    #[test]
    fn parse_verbose_markdown() {
        let content = verbose_markdown(40);
        let (headers, rows) = parse_markdown_table(&content);
        assert_eq!(headers, vec!["name", "age", "city", "status", "dept"]);
        assert_eq!(rows.len(), 40);
    }

    // Ragged tables must pass through verbatim (parity: c7665ca0). A row whose
    // cell count differs from the header can't be zipped without misattributing
    // columns (#1652), so parse_tabular returns None and the content is left as-is.
    #[test]
    fn parse_tabular_rejects_ragged_markdown() {
        let ragged = "| a | b | c |\n| --- | --- | --- |\n| 1 | 2 | 3 |\n| 4 | 5 |";
        assert!(parse_tabular(ragged).is_none());
    }

    #[test]
    fn parse_tabular_accepts_aligned_markdown() {
        let aligned = "| a | b | c |\n| --- | --- | --- |\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |";
        let parsed = parse_tabular(aligned).expect("aligned table should parse");
        assert_eq!(parsed.0, vec!["a", "b", "c"]);
        assert_eq!(parsed.1.len(), 2);
    }
}
