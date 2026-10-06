//! Inverse decoder for SmartCrusher's `csv-schema` densification format.
//!
//! The compactor ([`super::compactor`]) plus [`CsvSchemaFormatter`](super::formatter::CsvSchemaFormatter)
//! render a JSON array of objects into a compact `[N]{schema}` header followed
//! by CSV rows. That direction is encode-only in the engine — the original is
//! recoverable only by CCR retrieval of the stashed blob by hash. For a **live
//! agent** that never wants removal/CCR, the densified text must instead be
//! programmatically reversible so callers can verify losslessness
//! (densify → decode → original JSON) without a retrieval round-trip.
//!
//! This is a port of `headroom/transforms/compaction_codec.py` (`is_compacted`,
//! `contains_removal_marker`, `expand_compacted`). `factor_values` (the
//! encode-side value-factoring post-pass) is deliberately NOT ported: there is
//! no value-factoring encoder on the Rust side, and the decoder below already
//! accepts `__dict:K` legend blocks so Python-factored text still decodes.
//!
//! Native null encoding (matches the formatter, no formatter change):
//! bare `null` = JSON null, a bare empty cell = missing key, `""` = empty
//! string, `"null"` (quoted) = the literal string `"null"`.
//!
//! The decoder is total on success paths: malformed or lossy input (CCR
//! markers, dropped-row notes) yields `None`, never a panic. Two deliberate
//! deviations from the Python original, both on error paths only:
//!
//! - A `__dict:K` legend whose payload is not a JSON array returns `None`
//!   (Python indexes it and lets the resulting `TypeError` propagate).
//! - A `__key:` remainder is decoded as a CSV cell (inverting the writer's
//!   `json_scalar_to_csv` escaping); Python takes it raw.

use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;

/// Bare-cell token for JSON null. A quoted `"null"` is the literal string.
pub const NULL_TOKEN: &str = "null";

/// One parsed `{name:type}` schema declaration.
struct Column {
    name: String,
    col_type: String,
    #[allow(dead_code)]
    nullable: bool,
    path: Vec<String>,
}

impl Column {
    fn new(decl: &str) -> Self {
        // Split on the FINAL ':' so dotted names (meta.owner) survive.
        let (mut name, mut type_tag) = match decl.rfind(':') {
            Some(idx) => (&decl[..idx], &decl[idx + 1..]),
            None => (decl, "string"),
        };
        if name.is_empty() {
            name = type_tag;
            type_tag = "string";
        }
        let nullable = type_tag.ends_with('?');
        let type_tag = if nullable {
            &type_tag[..type_tag.len() - 1]
        } else {
            type_tag
        };
        // Unknown tag → opaque string; keeps the decoder total.
        let col_type = match type_tag {
            "string" | "int" | "float" | "bool" | "json" | "null" => type_tag,
            _ => "string",
        };
        Column {
            name: name.to_string(),
            col_type: col_type.to_string(),
            nullable,
            path: name.split('.').map(str::to_string).collect(),
        }
    }
}

/// Strip a JSON-string wrapper around a densified block.
///
/// When the router densifies a tool result whose content was a JSON array, it
/// re-serializes the compacted block as a JSON string, so the stored content
/// is `"[N]{...}\n..."` (leading quote, escaped newlines) rather than the
/// bare block. Peel that wrapper so the same codec handles both forms.
fn peel(text: &str) -> Cow<'_, str> {
    if text.starts_with('"')
        && let Ok(Value::String(inner)) = serde_json::from_str(text)
    {
        return Cow::Owned(inner);
    }
    Cow::Borrowed(text)
}

/// True if `text` looks like a `[N]{schema}` densified block.
///
/// Structural check (not a heuristic): the first non-empty line must open with
/// `[<digits>]{` and the brace section must close on that line. Accepts a
/// JSON-string-wrapped block. Cheap enough to gate [`expand_compacted`].
pub fn is_compacted(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let peeled = peel(text);
    let first = peeled.trim_start().split('\n').next().unwrap_or("");
    if !first.starts_with('[') {
        return false;
    }
    let rb = match first.find(']') {
        Some(i) => i,
        None => return false,
    };
    if rb < 2 || !first[1..rb].bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let after = &first[rb + 1..];
    after.starts_with('{') && after.contains('}')
}

/// True if the densified text removed content (CCR marker or dropped rows).
///
/// Either signal means the text alone cannot reconstruct the original — the
/// compression was lossy/removal-based, not pure densification.
pub fn contains_removal_marker(text: &str) -> bool {
    text.contains("<<ccr:") || text.contains("__dropped:")
}

/// Parse the header line: `[N]{c:t,c:t?}` optionally followed by
/// `__dict:K` (K value-factor legend lines follow the header) and/or
/// `__dropped:k`. Returns `(columns, declared_rows, dict_count)`, or `None`
/// when the line is not a well-formed header.
fn parse_header(line: &str) -> Option<(Vec<Column>, usize, usize)> {
    if !line.starts_with('[') {
        return None;
    }
    let rb = line.find(']')?;
    let open = line[rb..].find('{').map(|i| i + rb)?;
    let close = line[open..].find('}').map(|i| i + open)?;
    let declared: usize = line[1..rb].parse().ok()?;
    let body = &line[open + 1..close];
    let cols = if body.is_empty() {
        Vec::new()
    } else {
        body.split(',').map(Column::new).collect()
    };
    let mut dict_count = 0;
    for token in line[close + 1..].split_whitespace() {
        if let Some(k) = token.strip_prefix("__dict:") {
            dict_count = k.parse().unwrap_or(0);
        }
    }
    Some((cols, declared, dict_count))
}

/// Split a CSV `blob` into rows of `(was_quoted, value)` cells.
///
/// Hand-rolled rather than a CSV crate because the quoted-vs-bare
/// distinction must survive (`"null"` is a string; bare `null` is JSON null).
/// Quotes inside quoted fields are escaped by doubling, matching the
/// formatter's `csv_quote`; newlines inside quotes are honored. Tolerates
/// CRLF row endings. Operates on bytes: every delimiter is ASCII, so slicing
/// the original `&str` at those boundaries is always valid UTF-8.
fn iter_rows(blob: &str) -> Vec<Vec<(bool, String)>> {
    let b = blob.as_bytes();
    let n = b.len();
    let mut rows: Vec<Vec<(bool, String)>> = Vec::new();
    let mut row: Vec<(bool, String)> = Vec::new();
    let mut i = 0;
    while i < n {
        if b[i] == b'"' {
            // Quoted cell.
            i += 1;
            let mut buf: Vec<u8> = Vec::new();
            while i < n {
                if b[i] == b'"' {
                    if i + 1 < n && b[i + 1] == b'"' {
                        buf.push(b'"');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                buf.push(b[i]);
                i += 1;
            }
            row.push((true, String::from_utf8(buf).unwrap_or_default()));
            if i < n && b[i] == b'\r' {
                i += 1;
            }
            if i < n && b[i] == b',' {
                i += 1;
            } else if i < n && b[i] == b'\n' {
                rows.push(std::mem::take(&mut row));
                i += 1;
            }
        } else {
            // Bare cell up to ',' or newline.
            let mut j = i;
            while j < n && b[j] != b',' && b[j] != b'\n' {
                j += 1;
            }
            let mut cell = &blob[i..j];
            // A bare cell never legitimately holds '\r' (the formatter quotes
            // any value containing it), so a trailing '\r' is a CRLF artifact.
            if let Some(stripped) = cell.strip_suffix('\r') {
                cell = stripped;
            }
            row.push((false, cell.to_string()));
            if j < n && b[j] == b'\n' {
                rows.push(std::mem::take(&mut row));
            }
            i = j + 1;
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// Return the CSV body's data rows, dropping only a trailing-newline artifact.
///
/// The formatter newline-terminates every row, so a single-column row with a
/// missing key renders as a bare empty line — a legitimate `(false, "")` row,
/// NOT an artifact. Only a lone empty cell that EXCEEDS the declared row
/// count is the trailing-newline artifact and is dropped.
fn split_data_rows(blob: &str, declared: usize) -> Vec<Vec<(bool, String)>> {
    let mut rows: Vec<Vec<(bool, String)>> = iter_rows(blob)
        .into_iter()
        .filter(|r| !r.is_empty())
        .collect();
    let is_lone_empty = rows
        .last()
        .is_some_and(|r| r.len() == 1 && !r[0].0 && r[0].1.is_empty());
    if rows.len() == declared + 1 && is_lone_empty {
        rows.pop();
    }
    rows
}

/// Decode one cell. Returns `(present, value)`; `present == false` means the
/// key was absent (bare empty cell).
fn decode_cell(quoted: bool, value: &str, col: &Column) -> (bool, Value) {
    let as_string = |s: &str| (true, Value::String(s.to_string()));
    if quoted {
        if col.col_type == "json"
            && let Ok(v) = serde_json::from_str::<Value>(value)
        {
            return (true, v);
        }
        // Quoted "" -> "", quoted "null" -> "null", etc.
        return as_string(value);
    }
    if value.is_empty() {
        return (false, Value::Null);
    }
    if value == NULL_TOKEN {
        return (true, Value::Null);
    }
    match col.col_type.as_str() {
        "int" => match value.parse::<i64>().or_else(|_| {
            value
                .parse::<u64>()
                .map(|n| i64::try_from(n).unwrap_or(i64::MAX))
        }) {
            Ok(n) => (true, Value::from(n)),
            Err(_) => as_string(value),
        },
        "float" => match value.parse::<f64>() {
            Ok(f) if f.is_finite() => (true, Value::from(f)),
            _ => as_string(value),
        },
        "bool" => (true, Value::Bool(value == "true")),
        "json" => match serde_json::from_str::<Value>(value) {
            Ok(v) => (true, v),
            Err(_) => as_string(value),
        },
        // string / null / unknown
        _ => as_string(value),
    }
}

/// Set `val` at a possibly-dotted `path`, building nested objects.
/// A non-object at an intermediate key is replaced (mirrors the Python
/// original, which overwrites non-dicts the same way).
fn assign(obj: &mut Map<String, Value>, path: &[String], val: Value) {
    let mut cur = obj;
    for key in &path[..path.len().saturating_sub(1)] {
        cur = cur
            .entry(key.clone())
            .and_modify(|v| {
                if !v.is_object() {
                    *v = Value::Object(Map::new());
                }
            })
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("just ensured object");
    }
    if let Some(last) = path.last() {
        cur.insert(last.clone(), val);
    }
}

/// Reconstruct the original array of objects from densified `text`.
///
/// Returns the list of objects, or `None` if `text` is not a parseable
/// densified block or carries an irreversible removal marker (CCR / dropped
/// rows) — callers treat `None` as "cannot losslessly reverse".
pub fn expand_compacted(text: &str) -> Option<Vec<Value>> {
    if !is_compacted(text) || contains_removal_marker(text) {
        return None;
    }
    let text = peel(text);
    let (head, mut rest) = match text.find('\n') {
        Some(i) => (&text[..i], &text[i + 1..]),
        None => (text.as_ref(), ""),
    };
    let (cols, declared, dict_count) = parse_header(head.trim())?;

    // Value-factor legends: `__dict:K` declares K `@col=[...]` lines that
    // map a low-cardinality column's cells (integer indices) back to values.
    let mut legends: HashMap<String, Vec<Value>> = HashMap::new();
    for _ in 0..dict_count {
        let (line, tail) = match rest.find('\n') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        rest = tail;
        let (name, payload) = match line.find('=') {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => return None,
        };
        if !name.starts_with('@') {
            return None;
        }
        match serde_json::from_str::<Value>(payload) {
            Ok(Value::Array(items)) => {
                legends.insert(name[1..].to_string(), items);
            }
            _ => return None,
        }
    }

    let mut out: Vec<Value> = Vec::new();
    for cells in split_data_rows(rest, declared) {
        if cells.is_empty() {
            continue;
        }
        let mut record = Map::new();
        for (idx, col) in cols.iter().enumerate() {
            if idx >= cells.len() {
                break;
            }
            let (quoted, value) = &cells[idx];
            if legends.contains_key(&col.name) && !quoted {
                // Dict-encoded cell: null -> None, empty -> missing, else index.
                if value.is_empty() {
                    continue;
                }
                if value == NULL_TOKEN {
                    assign(&mut record, &col.path, Value::Null);
                    continue;
                }
                let legend_idx: usize = value.parse().ok()?;
                let decoded = legends
                    .get(&col.name)
                    .and_then(|l| l.get(legend_idx))
                    .cloned()?;
                assign(&mut record, &col.path, decoded);
                continue;
            }
            let (present, decoded) = decode_cell(*quoted, value, col);
            if present {
                assign(&mut record, &col.path, decoded);
            }
        }
        out.push(Value::Object(record));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::super::compactor::{CompactConfig, compact};
    use super::super::formatter::{CsvSchemaFormatter, Formatter};
    use super::*;
    use serde_json::json;

    /// Densify `records` through the real compactor + csv-schema renderer,
    /// mirroring the upstream codec tests (`SmartCrusher().crush_array_json`
    /// → `compacted`). `None` when the array does not densify.
    fn densify(records: &[Value]) -> Option<String> {
        let c = compact(records, &CompactConfig::default());
        if !c.was_compacted() {
            return None;
        }
        Some(CsvSchemaFormatter::default().format(&c))
    }

    fn records(v: Value) -> Vec<Value> {
        v.as_array().cloned().unwrap_or_default()
    }

    #[test]
    fn round_trips_uniform_table() {
        let original = records(json!([
            {"id": 0, "owner": "user0", "score": 0.0, "active": true},
            {"id": 1, "owner": "user1", "score": 0.3333333333333333, "active": false},
            {"id": 2, "owner": "user2", "score": 0.6666666666666666, "active": true},
            {"id": 3, "owner": "user0", "score": 1.0, "active": false},
        ]));
        let text = densify(&original).expect("uniform table densifies");
        assert!(is_compacted(&text));
        assert_eq!(expand_compacted(&text), Some(original));
    }

    #[test]
    fn null_empty_missing_and_literal_null_stay_distinct() {
        let original = records(json!([
            {"a": 0, "b": 0, "c": null},
            {"a": 1, "b": 1, "c": ""},
            {"a": 2, "b": 2},
            {"a": 3, "b": 3, "c": "\\N"},
            {"a": 4, "b": 4, "c": "null"},
            {"a": 5, "b": 5, "c": "v5"},
        ]));
        let text = densify(&original).expect("table densifies");
        let decoded = expand_compacted(&text).expect("decodes");
        assert!(!decoded[2].as_object().unwrap().contains_key("c"));
        assert_eq!(decoded[0]["c"], Value::Null);
        assert_eq!(decoded[1]["c"], json!(""));
        assert_eq!(decoded[3]["c"], json!("\\N"));
        assert_eq!(decoded[4]["c"], json!("null"));
        assert_eq!(decoded, original);
    }

    #[test]
    fn strings_with_commas_quotes_and_unicode_round_trip() {
        let original = records(json!([
            {"k": 0, "v": "a,b"},
            {"k": 1, "v": "he said \"hi\""},
            {"k": 2, "v": "café 日本語 🔥"},
            {"k": 3, "v": "trailing space "},
            {"k": 4, "v": ""},
            {"k": 5, "v": "line1\nline2"},
        ]));
        let text = densify(&original).expect("table densifies");
        assert_eq!(expand_compacted(&text), Some(original));
    }

    #[test]
    fn nested_objects_flatten_to_dotted_columns_and_round_trip() {
        let original = records(json!([
            {"id": 0, "meta": {"owner": "u0", "rank": 0}},
            {"id": 1, "meta": {"owner": "u1", "rank": 1}},
            {"id": 2, "meta": {"owner": "u2", "rank": 2}},
            {"id": 3, "meta": {"owner": "u0", "rank": 3}},
        ]));
        let text = densify(&original).expect("nested table densifies");
        assert!(
            text.contains("meta.owner"),
            "nested flattens to dotted columns"
        );
        assert_eq!(expand_compacted(&text), Some(original));
    }

    #[test]
    fn bool_int_and_float_types_survive() {
        // Guards the bool-vs-int conflation the Python side pins in
        // test_verify_rejects_bool_int_conflation: a bool column decodes to
        // bools, an int column to ints, never crossed.
        let original = records(json!([
            {"ok": true, "n": 1, "f": 1.5},
            {"ok": false, "n": 0, "f": 2.5},
            {"ok": true, "n": -7, "f": 0.0},
        ]));
        let text = densify(&original).expect("table densifies");
        let decoded = expand_compacted(&text).expect("decodes");
        assert_eq!(decoded, original);
        assert!(decoded[0]["ok"].is_boolean());
        assert!(decoded[0]["n"].is_i64());
        assert!(decoded[0]["f"].is_f64());
    }

    #[test]
    fn json_string_wrapped_block_is_recognized() {
        let original = records(json!([
            {"id": 0, "tag": "t0"},
            {"id": 1, "tag": "t1"},
            {"id": 2, "tag": "t2"},
        ]));
        let bare = densify(&original).expect("table densifies");
        let wrapped = serde_json::to_string(&bare).unwrap();
        assert!(is_compacted(&wrapped));
        assert_eq!(expand_compacted(&wrapped), Some(original));
    }

    #[test]
    fn crlf_line_endings_round_trip() {
        let original = records(json!([
            {"id": 0, "name": "n0", "v": "val 0"},
            {"id": 1, "name": "n1", "v": "val 1"},
            {"id": 2, "name": "n2", "v": "val 2"},
        ]));
        let bare = densify(&original).expect("table densifies");
        let crlf = bare.replace('\n', "\r\n");
        assert_eq!(expand_compacted(&crlf), Some(original.clone()));
        assert_eq!(expand_compacted(&crlf), expand_compacted(&bare));
    }

    #[test]
    fn value_factor_legends_decode() {
        // No Rust encoder emits __dict: blocks; the decoder still accepts
        // them so Python-factored text reverses.
        let text =
            "[3]{path:string,line:int} __dict:1\n@path=[\"./a.py\",\"./b.py\"]\n0,1\n1,2\n0,3\n";
        let decoded = expand_compacted(text).expect("decodes");
        assert_eq!(
            decoded,
            records(json!([
                {"path": "./a.py", "line": 1},
                {"path": "./b.py", "line": 2},
                {"path": "./a.py", "line": 3},
            ]))
        );
    }

    #[test]
    fn removal_marker_makes_text_unreversible() {
        let text = "[5]{a:int,blob:string}\n0,<<ccr:abc123,string,1.2KB>>\n";
        assert!(contains_removal_marker(text));
        assert_eq!(expand_compacted(text), None);
        assert!(contains_removal_marker("[2]{a:int} __dropped:1\n0\n1\n"));
        assert_eq!(expand_compacted("[2]{a:int} __dropped:1\n0\n1\n"), None);
    }

    #[test]
    fn non_compacted_text_is_ignored() {
        assert!(!is_compacted("just some prose, not densified"));
        assert_eq!(expand_compacted("just some prose"), None);
        assert!(!is_compacted(""));
        assert!(!is_compacted("[x]{a:int}\n"));
        assert!(!is_compacted("[3]no-brace\n"));
    }

    #[test]
    fn single_column_edge_rows_follow_declared_count() {
        // Missing key renders as a bare empty line; the declared row count
        // keeps the fourth row from being dropped as a trailing artifact.
        let block = "[4]{k:string?}\n\"\"\nnull\n\"null\"\n\n";
        assert_eq!(
            expand_compacted(block),
            Some(records(json!([{"k": ""}, {"k": null}, {"k": "null"}, {}])))
        );
        // A blank line BEYOND the declared count is the trailing-newline
        // artifact, not a spurious extra row.
        assert_eq!(
            expand_compacted("[2]{k:string?}\na\nb\n\n"),
            Some(records(json!([{"k": "a"}, {"k": "b"}])))
        );
    }

    #[test]
    fn densify_is_byte_stable_for_cache_safety() {
        // Densification must be deterministic: the same records render
        // byte-identical output on every turn, so a shared prefix keeps its
        // prompt-cache hash and rows only get denser, never reworded.
        let original = records(json!([
            {"id": 0, "owner": "user0", "score": 1.5, "active": true},
            {"id": 1, "owner": "user1", "score": 2.5, "active": false},
            {"id": 2, "owner": "user0", "score": 3.5, "active": true},
            {"id": 3, "owner": "user1", "score": 4.5, "active": false},
        ]));
        let first = densify(&original).expect("densifies");
        for _ in 0..3 {
            assert_eq!(densify(&original), Some(first.clone()));
        }
        // And the stable bytes still reverse to the original.
        assert_eq!(expand_compacted(&first), Some(original));
    }

    #[test]
    fn malformed_blocks_return_none() {
        assert_eq!(expand_compacted("[3]{a:int"), None); // no closing brace
        // Declared legend line without '='.
        assert_eq!(
            expand_compacted("[2]{a:string} __dict:1\nnot-a-legend\n0\n1\n"),
            None
        );
        // Legend payload is not a JSON array.
        assert_eq!(expand_compacted("[1]{a:string} __dict:1\n@a=42\n0\n"), None);
        // Legend index out of range.
        assert_eq!(
            expand_compacted("[1]{a:string} __dict:1\n@a=[\"x\"]\n5\n"),
            None
        );
    }
}
