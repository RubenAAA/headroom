//! Differential network capture reporting (Rust port of
//! `headroom/capture/network_diff.py`). Compares two JSONL capture files
//! (direct Claude Code lane vs Headroom-proxied lane) and renders a Markdown
//! and/or JSON diff report.
//!
//! Intended deviations from Python: `request_body_preview` is not ported
//! (nothing in the diff or the reports reads it), and `response_status` /
//! JSON-value equality follow serde_json semantics (Python would treat
//! `1 == 1.0`; serde_json with arbitrary_precision keeps them distinct).

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

type Error = Box<dyn std::error::Error>;

const SENSITIVE_HEADER_PARTS: &[&str] = &[
    "authorization",
    "api-key",
    "apikey",
    "token",
    "secret",
    "cookie",
];
const SENSITIVE_QUERY_PARTS: &[&str] = &["key", "token", "secret", "signature", "code"];

/// A sanitized HTTP request/response pair captured by the harness.
pub struct CapturedExchange {
    pub lane: String,
    pub sequence: i64,
    pub method: String,
    pub url: String,
    pub host: String,
    pub path: String,
    pub request_headers: Vec<(String, String)>,
    pub response_status: Value,
    pub response_headers: Vec<(String, String)>,
    pub request_body_sha256: Option<String>,
    pub request_body_size: i64,
    pub request_json: Option<Value>,
}

impl CapturedExchange {
    pub fn route_key(&self) -> String {
        format!("{} {}{}", self.method.to_uppercase(), self.host, self.path)
    }

    pub fn path_key(&self) -> String {
        format!("{} {}", self.method.to_uppercase(), self.path)
    }
}

/// Comparison result between a direct lane and a Headroom lane.
pub struct CaptureDiff {
    pub direct_count: usize,
    pub headroom_count: usize,
    pub only_direct: Vec<String>,
    pub only_headroom: Vec<String>,
    pub paired: Vec<Value>,
    pub generated_at: String,
}

impl CaptureDiff {
    pub fn to_dict(&self) -> Value {
        json!({
            "generated_at": self.generated_at,
            "direct_count": self.direct_count,
            "headroom_count": self.headroom_count,
            "only_direct": self.only_direct,
            "only_headroom": self.only_headroom,
            "paired": self.paired,
        })
    }
}

// ─── URL and header sanitization ─────────────────────────────────────────

/// Python `str(value)` for the JSON values a capture record can hold.
fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python truthiness for JSON values.
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

fn redact_value(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    let text = py_str(value);
    if text.is_empty() {
        text
    } else {
        "<redacted>".to_string()
    }
}

fn sanitize_headers(headers: Option<&Value>) -> Vec<(String, String)> {
    let Some(Value::Object(map)) = headers else {
        return Vec::new();
    };
    map.iter()
        .map(|(key, value)| {
            let lower = key.to_lowercase();
            if SENSITIVE_HEADER_PARTS
                .iter()
                .any(|part| lower.contains(part))
            {
                (key.clone(), redact_value(value))
            } else {
                (key.clone(), py_str(value))
            }
        })
        .collect()
}

/// Minimal `urllib.parse.urlsplit` for absolute capture URLs: returns
/// (scheme, netloc, path, query) with the fragment dropped.
fn urlsplit(url: &str) -> (&str, &str, &str, &str) {
    let url = url.split('#').next().unwrap_or("");
    let (scheme, rest) = match url.find("://") {
        Some(idx) => (&url[..idx], &url[idx + 3..]),
        None => ("", url),
    };
    let (authority_and_path, query) = match rest.split_once('?') {
        Some((a, q)) => (a, q),
        None => (rest, ""),
    };
    if scheme.is_empty() {
        return ("", "", authority_and_path, query);
    }
    let (netloc, path) = match authority_and_path.find('/') {
        Some(idx) => (&authority_and_path[..idx], &authority_and_path[idx..]),
        None => (authority_and_path, ""),
    };
    (scheme, netloc, path, query)
}

/// `urllib.parse.unquote_plus`: '+' → space, %XX → byte, lossy UTF-8.
fn unquote_plus(text: &str) -> String {
    let raw = text.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'+' => bytes.push(b' '),
            b'%' if i + 2 < raw.len()
                && raw[i + 1].is_ascii_hexdigit()
                && raw[i + 2].is_ascii_hexdigit() =>
            {
                let hex = std::str::from_utf8(&raw[i + 1..i + 3]).unwrap();
                bytes.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
                continue;
            }
            b => bytes.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `urllib.parse.quote_plus` with the default safe set.
fn quote_plus(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Redact sensitive query parameters and drop the fragment (mirrors
/// Python's parse_qsl(keep_blank_values=True) + urlencode round trip).
pub fn sanitize_url(url: &str) -> String {
    let (scheme, netloc, path, query) = urlsplit(url);
    let mut pairs: Vec<(String, String)> = Vec::new();
    for field in query.split('&') {
        if field.is_empty() {
            continue;
        }
        let (key, value) = match field.split_once('=') {
            Some((k, v)) => (unquote_plus(k), unquote_plus(v)),
            None => (unquote_plus(field), String::new()),
        };
        let lower = key.to_lowercase();
        if SENSITIVE_QUERY_PARTS
            .iter()
            .any(|part| lower.contains(part))
        {
            pairs.push((key, "<redacted>".to_string()));
        } else {
            pairs.push((key, value));
        }
    }
    let encoded: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v)))
        .collect();
    let query = encoded.join("&");
    // urlunsplit((scheme, netloc, path, query, "")).
    let mut result = String::new();
    if !scheme.is_empty() {
        result.push_str(scheme);
        result.push_str("://");
    }
    result.push_str(netloc);
    result.push_str(path);
    if !query.is_empty() {
        result.push('?');
        result.push_str(&query);
    }
    result
}

// ─── Record loading ──────────────────────────────────────────────────────

fn body_bytes(record: &Map<String, Value>) -> Vec<u8> {
    if let Some(Value::String(body_b64)) = record.get("request_body_b64") {
        return base64::engine::general_purpose::STANDARD
            .decode(body_b64)
            .unwrap_or_default();
    }
    if let Some(Value::String(body)) = record.get("request_body") {
        return body.as_bytes().to_vec();
    }
    Vec::new()
}

/// Build a sanitized exchange from one JSONL capture record.
pub fn exchange_from_record(
    record: &Map<String, Value>,
    fallback_lane: &str,
    sequence: i64,
) -> CapturedExchange {
    let str_or = |key: &str, fallback: &str| -> String {
        match record.get(key) {
            Some(v) if truthy(v) => py_str(v),
            _ => fallback.to_string(),
        }
    };
    let url = sanitize_url(&str_or("url", ""));
    let (_, netloc, raw_path, query) = urlsplit(&url);
    let mut path = if raw_path.is_empty() { "/" } else { raw_path }.to_string();
    if !query.is_empty() {
        path = format!("{path}?{query}");
    }
    let body = body_bytes(record);
    let request_json = match record.get("request_json") {
        Some(v) if !v.is_null() => Some(v.clone()),
        _ => {
            if body.is_empty() {
                None
            } else {
                serde_json::from_slice(&body).ok()
            }
        }
    };
    let body_sha = match record.get("request_body_sha256") {
        Some(v) if !v.is_null() => {
            let text = py_str(v);
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => {
            if body.is_empty() {
                None
            } else {
                Some(hex::encode(Sha256::digest(&body)))
            }
        }
    };
    let body_size = match record.get("request_body_size") {
        Some(v) if truthy(v) => v.as_i64().unwrap_or(body.len() as i64),
        _ => body.len() as i64,
    };
    let host = if netloc.is_empty() {
        str_or("host", "")
    } else {
        netloc.to_string()
    };
    CapturedExchange {
        lane: str_or("lane", fallback_lane),
        sequence: match record.get("sequence") {
            Some(v) if truthy(v) => v.as_i64().unwrap_or(sequence),
            _ => sequence,
        },
        method: str_or("method", "GET").to_uppercase(),
        url,
        host,
        path,
        request_headers: sanitize_headers(record.get("request_headers")),
        response_status: record
            .get("response_status")
            .cloned()
            .unwrap_or(Value::Null),
        response_headers: sanitize_headers(record.get("response_headers")),
        request_body_sha256: body_sha,
        request_body_size: body_size,
        request_json,
    }
}

/// Load a JSONL capture file produced by the mitmproxy addon. Malformed
/// lines are skipped (captures can be truncated mid-write).
pub fn load_capture_file(path: &Path, fallback_lane: &str) -> Result<Vec<CapturedExchange>, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut exchanges = Vec::new();
    let mut skipped = 0usize;
    for (line_number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(record)) => exchanges.push(exchange_from_record(
                &record,
                fallback_lane,
                (line_number + 1) as i64,
            )),
            Ok(other) => {
                // Python would raise on non-dict records; a scalar JSON line
                // is effectively malformed for our purposes.
                let _ = other;
                skipped += 1;
            }
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        eprintln!(
            "Skipped {skipped} malformed line(s) in capture file {}",
            path.display()
        );
    }
    Ok(exchanges)
}

// ─── Diffing ─────────────────────────────────────────────────────────────

/// Flatten a JSON value into `$.path` → leaf-value pairs.
fn json_paths(value: &Value, prefix: &str, out: &mut BTreeMap<String, Value>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                json_paths(&map[key.as_str()], &format!("{prefix}.{key}"), out);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for (index, child) in items.iter().enumerate() {
                json_paths(child, &format!("{prefix}[{index}]"), out);
            }
        }
        _ => {
            // Empty objects/arrays and scalars are all leaves.
            out.insert(prefix.to_string(), value.clone());
        }
    }
}

fn header_delta(
    direct: &[(String, String)],
    headroom: &[(String, String)],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let direct_keys: BTreeMap<String, &String> =
        direct.iter().map(|(k, _)| (k.to_lowercase(), k)).collect();
    let headroom_keys: BTreeMap<String, &String> = headroom
        .iter()
        .map(|(k, _)| (k.to_lowercase(), k))
        .collect();
    let lookup = |headers: &[(String, String)], lower: &str| -> Option<String> {
        headers
            .iter()
            .rev()
            .find(|(k, _)| k.to_lowercase() == lower)
            .map(|(_, v)| v.clone())
    };
    let mut only_direct: Vec<String> = direct_keys
        .iter()
        .filter(|(lower, _)| !headroom_keys.contains_key(*lower))
        .map(|(_, key)| (*key).clone())
        .collect();
    only_direct.sort();
    let mut only_headroom: Vec<String> = headroom_keys
        .iter()
        .filter(|(lower, _)| !direct_keys.contains_key(*lower))
        .map(|(_, key)| (*key).clone())
        .collect();
    only_headroom.sort();
    let mut changed: Vec<String> = Vec::new();
    for (lower, d_key) in &direct_keys {
        if headroom_keys.contains_key(lower) && lookup(direct, lower) != lookup(headroom, lower) {
            changed.push((*d_key).clone());
        }
    }
    (only_direct, only_headroom, changed)
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    let target = name.to_lowercase();
    headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == target)
        .map(|(_, v)| v.clone())
}

/// Recursively sort object keys so compact serialization matches Python's
/// `json.dumps(..., sort_keys=True)`.
fn sorted_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), sorted_value(&map[key.as_str()]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_value).collect()),
        other => other.clone(),
    }
}

/// Byte length of Python's `json.dumps(value, sort_keys=True,
/// separators=(",", ":"))` — ensure_ascii escapes non-ASCII as \uXXXX.
fn python_compact_json_len(value: &Value) -> i64 {
    let compact = serde_json::to_string(&sorted_value(value)).unwrap_or_default();
    compact
        .chars()
        .map(|c| {
            if c.is_ascii() {
                1
            } else if (c as u32) <= 0xFFFF {
                6 // \uXXXX
            } else {
                12 // surrogate pair
            }
        })
        .sum()
}

fn anthropic_request_summary(exchange: &CapturedExchange) -> Value {
    let (tool_count, tool_bytes) = match &exchange.request_json {
        Some(Value::Object(map)) => match map.get("tools") {
            Some(tools @ Value::Array(items)) => {
                (items.len() as i64, python_compact_json_len(tools))
            }
            _ => (0, 0),
        },
        _ => (0, 0),
    };
    json!({
        "anthropic_beta": header_value(&exchange.request_headers, "anthropic-beta"),
        "tools_count": tool_count,
        "tools_bytes": tool_bytes,
    })
}

type Pairing<'a> = (
    Vec<(&'a CapturedExchange, &'a CapturedExchange)>,
    Vec<String>,
    Vec<String>,
);

fn pair_exchanges<'a>(
    direct: &'a [CapturedExchange],
    headroom: &'a [CapturedExchange],
    pair_by: &str,
) -> Pairing<'a> {
    let key_of = |item: &CapturedExchange| -> String {
        if pair_by == "route" {
            item.route_key()
        } else {
            item.path_key()
        }
    };
    let mut direct_by_key: BTreeMap<String, Vec<&CapturedExchange>> = BTreeMap::new();
    let mut headroom_by_key: BTreeMap<String, Vec<&CapturedExchange>> = BTreeMap::new();
    for item in direct {
        direct_by_key.entry(key_of(item)).or_default().push(item);
    }
    for item in headroom {
        headroom_by_key.entry(key_of(item)).or_default().push(item);
    }
    let mut keys: Vec<&String> = direct_by_key.keys().chain(headroom_by_key.keys()).collect();
    keys.sort();
    keys.dedup();

    let mut pairs = Vec::new();
    let mut only_direct = Vec::new();
    let mut only_headroom = Vec::new();
    static EMPTY: Vec<&CapturedExchange> = Vec::new();
    for key in keys {
        let direct_items = direct_by_key.get(key.as_str()).unwrap_or(&EMPTY);
        let headroom_items = headroom_by_key.get(key.as_str()).unwrap_or(&EMPTY);
        let shared = direct_items.len().min(headroom_items.len());
        pairs.extend(
            direct_items[..shared]
                .iter()
                .zip(&headroom_items[..shared])
                .map(|(d, h)| (*d, *h)),
        );
        only_direct.extend(direct_items[shared..].iter().map(|item| item.route_key()));
        only_headroom.extend(headroom_items[shared..].iter().map(|item| item.route_key()));
    }
    (pairs, only_direct, only_headroom)
}

/// Compare the two capture lanes and build the structured diff.
pub fn compare_captures(
    direct: &[CapturedExchange],
    headroom: &[CapturedExchange],
    pair_by: &str,
) -> CaptureDiff {
    let (pairs, only_direct, only_headroom) = pair_exchanges(direct, headroom, pair_by);
    let mut paired = Vec::new();
    for (direct_item, headroom_item) in pairs {
        let mut direct_paths = BTreeMap::new();
        if let Some(value) = &direct_item.request_json {
            json_paths(value, "$", &mut direct_paths);
        }
        let mut headroom_paths = BTreeMap::new();
        if let Some(value) = &headroom_item.request_json {
            json_paths(value, "$", &mut headroom_paths);
        }
        let only_direct_json: Vec<&String> = direct_paths
            .keys()
            .filter(|path| !headroom_paths.contains_key(*path))
            .collect();
        let only_headroom_json: Vec<&String> = headroom_paths
            .keys()
            .filter(|path| !direct_paths.contains_key(*path))
            .collect();
        let changed_json: Vec<&String> = direct_paths
            .iter()
            .filter(|(path, value)| {
                headroom_paths
                    .get(*path)
                    .map(|other| other != *value)
                    .unwrap_or(false)
            })
            .map(|(path, _)| path)
            .collect();
        let (headers_only_direct, headers_only_headroom, headers_changed) =
            header_delta(&direct_item.request_headers, &headroom_item.request_headers);
        paired.push(json!({
            "route": direct_item.route_key(),
            "headroom_route": headroom_item.route_key(),
            "direct_sequence": direct_item.sequence,
            "headroom_sequence": headroom_item.sequence,
            "status": {
                "direct": direct_item.response_status,
                "headroom": headroom_item.response_status,
            },
            "request_body_size": {
                "direct": direct_item.request_body_size,
                "headroom": headroom_item.request_body_size,
                "delta": headroom_item.request_body_size - direct_item.request_body_size,
            },
            "request_body_sha256": {
                "direct": direct_item.request_body_sha256,
                "headroom": headroom_item.request_body_sha256,
                "same": direct_item.request_body_sha256 == headroom_item.request_body_sha256,
            },
            "anthropic": {
                "direct": anthropic_request_summary(direct_item),
                "headroom": anthropic_request_summary(headroom_item),
            },
            "headers": {
                "only_direct": headers_only_direct,
                "only_headroom": headers_only_headroom,
                "changed": headers_changed,
            },
            "json": {
                "only_direct": only_direct_json,
                "only_headroom": only_headroom_json,
                "changed": changed_json,
            },
        }));
    }
    CaptureDiff {
        direct_count: direct.len(),
        headroom_count: headroom.len(),
        only_direct,
        only_headroom,
        paired,
        generated_at: chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.6f+00:00")
            .to_string(),
    }
}

// ─── Markdown rendering ──────────────────────────────────────────────────

fn list_or_dash(values: &Value) -> String {
    let items: Vec<&str> = values
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(", ")
    }
}

pub fn render_markdown_report(diff: &CaptureDiff) -> String {
    let mut lines = vec![
        "# Differential Network Capture Report".to_string(),
        String::new(),
        format!("Generated: `{}`", diff.generated_at),
        String::new(),
        "## Summary".to_string(),
        String::new(),
        format!("- Direct exchanges: `{}`", diff.direct_count),
        format!("- Headroom exchanges: `{}`", diff.headroom_count),
        format!("- Paired exchanges: `{}`", diff.paired.len()),
        format!("- Only direct: `{}`", diff.only_direct.len()),
        format!("- Only Headroom: `{}`", diff.only_headroom.len()),
        String::new(),
    ];
    if !diff.only_direct.is_empty() {
        lines.push("## Only Direct".to_string());
        lines.push(String::new());
        lines.extend(diff.only_direct.iter().map(|route| format!("- `{route}`")));
        lines.push(String::new());
    }
    if !diff.only_headroom.is_empty() {
        lines.push("## Only Headroom".to_string());
        lines.push(String::new());
        lines.extend(
            diff.only_headroom
                .iter()
                .map(|route| format!("- `{route}`")),
        );
        lines.push(String::new());
    }

    lines.extend([
        "## Paired Exchanges".to_string(),
        String::new(),
        "| Route | Status | Body Bytes | Body SHA | Header Delta | JSON Delta |".to_string(),
        "| --- | --- | ---: | --- | --- | --- |".to_string(),
    ]);
    for item in &diff.paired {
        let mut route = item["route"].as_str().unwrap_or("").to_string();
        let headroom_route = item["headroom_route"].as_str().unwrap_or("");
        if !headroom_route.is_empty() && headroom_route != route {
            route = format!("{route} -> {headroom_route}");
        }
        let status = format!(
            "{} -> {}",
            py_str(&item["status"]["direct"]),
            py_str(&item["status"]["headroom"])
        );
        let sizes = &item["request_body_size"];
        let body = format!(
            "{} -> {} ({:+})",
            py_str(&sizes["direct"]),
            py_str(&sizes["headroom"]),
            sizes["delta"].as_i64().unwrap_or(0)
        );
        let sha = if item["request_body_sha256"]["same"]
            .as_bool()
            .unwrap_or(false)
        {
            "same"
        } else {
            "changed"
        };
        let headers = &item["headers"];
        let header_delta = format!(
            "+{}; -{}; changed={}",
            list_or_dash(&headers["only_headroom"]),
            list_or_dash(&headers["only_direct"]),
            list_or_dash(&headers["changed"])
        );
        let json_delta = &item["json"];
        let direct_anthropic = &item["anthropic"]["direct"];
        let headroom_anthropic = &item["anthropic"]["headroom"];
        let tool_delta = headroom_anthropic["tools_bytes"].as_i64().unwrap_or(0)
            - direct_anthropic["tools_bytes"].as_i64().unwrap_or(0);
        let json_text = format!(
            "+{}; -{}; changed={}; tools={}->{} ({:+} bytes)",
            list_or_dash(&json_delta["only_headroom"]),
            list_or_dash(&json_delta["only_direct"]),
            list_or_dash(&json_delta["changed"]),
            direct_anthropic["tools_count"].as_i64().unwrap_or(0),
            headroom_anthropic["tools_count"].as_i64().unwrap_or(0),
            tool_delta
        );
        lines.push(format!(
            "| `{route}` | `{status}` | `{body}` | `{sha}` | {header_delta} | {json_text} |"
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_from(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => unreachable!(),
        }
    }

    fn body_b64(payload: &Value) -> String {
        base64::engine::general_purpose::STANDARD.encode(payload.to_string())
    }

    #[test]
    fn sanitize_url_redacts_sensitive_query_params() {
        assert_eq!(
            sanitize_url("https://api.anthropic.com/v1/messages?api_key=secret"),
            "https://api.anthropic.com/v1/messages?api_key=%3Credacted%3E"
        );
        // Fragment dropped, non-sensitive values re-encoded like urlencode.
        assert_eq!(
            sanitize_url("https://x.test/p?a=b%20c&flag#frag"),
            "https://x.test/p?a=b+c&flag="
        );
        assert_eq!(sanitize_url("https://x.test/p"), "https://x.test/p");
        assert_eq!(sanitize_url(""), "");
    }

    #[test]
    fn sanitize_headers_redacts_sensitive_parts() {
        let headers = json!({
            "authorization": "Bearer secret",
            "X-Api-Key": "abc",
            "anthropic-version": "2023-06-01",
        });
        let sanitized = sanitize_headers(Some(&headers));
        assert_eq!(
            sanitized,
            vec![
                ("authorization".to_string(), "<redacted>".to_string()),
                ("X-Api-Key".to_string(), "<redacted>".to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ]
        );
    }

    #[test]
    fn json_paths_flattens_with_sorted_keys_and_empties() {
        let value = json!({"b": [1, {"x": null}], "a": {}, "c": "s"});
        let mut paths = BTreeMap::new();
        json_paths(&value, "$", &mut paths);
        let keys: Vec<&str> = paths.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["$.a", "$.b[0]", "$.b[1].x", "$.c"]);
        assert_eq!(paths["$.a"], json!({}));
    }

    #[test]
    fn python_compact_json_len_matches_ensure_ascii() {
        // json.dumps([{"b":1,"a":"é"}], sort_keys=True, separators=(",",":"))
        // → '[{"a":"é","b":1}]' → 22 bytes.
        assert_eq!(python_compact_json_len(&json!([{"b": 1, "a": "é"}])), 22);
        assert_eq!(python_compact_json_len(&json!([])), 2);
    }

    #[test]
    fn diff_redacts_and_reports_body_json_deltas() {
        // Mirror of Python's
        // test_network_diff_redacts_and_reports_body_json_deltas.
        let direct_record = record_from(json!({
            "lane": "direct",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages?api_key=secret",
            "request_headers": {
                "authorization": "Bearer secret",
                "anthropic-version": "2023-06-01",
                "anthropic-beta": "deferred-tools",
            },
            "request_body_b64": body_b64(
                &json!({"model": "claude", "messages": [{"content": "hi"}], "tools": []})
            ),
            "response_status": 200,
        }));
        let headroom_record = record_from(json!({
            "lane": "headroom",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages?api_key=secret",
            "request_headers": {
                "authorization": "Bearer other",
                "anthropic-version": "2023-06-01",
                "x-headroom-mode": "optimize",
            },
            "request_body_b64": body_b64(&json!({
                "model": "claude",
                "messages": [{"content": "hello"}],
                "metadata": {},
                "tools": [{"name": "ctx_execute", "input_schema": {"type": "object"}}],
            })),
            "response_status": 200,
        }));
        let direct = vec![exchange_from_record(&direct_record, "direct", 1)];
        let headroom = vec![exchange_from_record(&headroom_record, "headroom", 1)];

        assert_eq!(
            direct[0].url,
            "https://api.anthropic.com/v1/messages?api_key=%3Credacted%3E"
        );
        assert_eq!(
            header_value(&direct[0].request_headers, "authorization").as_deref(),
            Some("<redacted>")
        );

        let diff = compare_captures(&direct, &headroom, "path");
        assert_eq!(diff.direct_count, 1);
        assert_eq!(diff.headroom_count, 1);
        let paired = &diff.paired[0];
        assert_eq!(
            paired["headers"]["only_headroom"],
            json!(["x-headroom-mode"])
        );
        assert!(paired["json"]["only_headroom"]
            .as_array()
            .unwrap()
            .contains(&json!("$.metadata")));
        assert!(paired["json"]["changed"]
            .as_array()
            .unwrap()
            .contains(&json!("$.messages[0].content")));
        assert_eq!(paired["anthropic"]["direct"]["tools_count"], json!(0));
        assert_eq!(paired["anthropic"]["headroom"]["tools_count"], json!(1));

        let markdown = render_markdown_report(&diff);
        assert!(markdown.contains("Differential Network Capture Report"));
        assert!(markdown.contains("POST api.anthropic.com/v1/messages?api_key=%3Credacted%3E"));
        assert!(markdown.contains("tools=0->1"));
    }

    #[test]
    fn load_capture_file_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.jsonl");
        std::fs::write(
            &path,
            "{\"method\": \"GET\", \"url\": \"https://x.test/a\"}\nnot json\n\n{\"url\": \"https://x.test/b\"}\n",
        )
        .unwrap();
        let exchanges = load_capture_file(&path, "direct").unwrap();
        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].sequence, 1);
        assert_eq!(exchanges[1].sequence, 4);
        assert_eq!(exchanges[1].method, "GET");
        assert_eq!(exchanges[1].lane, "direct");
    }

    #[test]
    fn unpaired_exchanges_listed_by_route_key() {
        let a = record_from(json!({"method": "GET", "url": "https://x.test/only-direct"}));
        let b = record_from(json!({"method": "GET", "url": "https://x.test/shared"}));
        let c = record_from(json!({"method": "GET", "url": "https://y.test/shared"}));
        let direct = vec![
            exchange_from_record(&a, "direct", 1),
            exchange_from_record(&b, "direct", 2),
        ];
        let headroom = vec![exchange_from_record(&c, "headroom", 1)];
        // pair-by path: /shared pairs across hosts; /only-direct is unpaired.
        let diff = compare_captures(&direct, &headroom, "path");
        assert_eq!(diff.only_direct, vec!["GET x.test/only-direct"]);
        assert!(diff.only_headroom.is_empty());
        assert_eq!(diff.paired.len(), 1);
        assert_eq!(diff.paired[0]["route"], json!("GET x.test/shared"));
        assert_eq!(diff.paired[0]["headroom_route"], json!("GET y.test/shared"));
        // pair-by route: nothing pairs.
        let diff = compare_captures(&direct, &headroom, "route");
        assert_eq!(diff.paired.len(), 0);
        assert_eq!(
            diff.only_direct,
            vec!["GET x.test/only-direct", "GET x.test/shared"]
        );
        assert_eq!(diff.only_headroom, vec!["GET y.test/shared"]);
    }
}
