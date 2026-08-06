//! Message parsing — Rust port of `headroom/parser.py`.
//!
//! Turns a request's messages into [`Block`]s: the atomic unit the rest of the
//! analysis layer reasons about. A block carries its text, a token estimate, a
//! content hash for grouping, the index of the message it came from, and flags
//! (tool identity, per-block waste signals).
//!
//! On top of the per-message split, [`parse_messages`] runs two cross-message
//! passes that produce the re-read waste signal:
//!
//! - **Content re-read** — the same tool-result bytes served at more than one
//!   message position means the agent re-fetched something it already had.
//! - **Re-issued call** — the same tool invoked with the same arguments again,
//!   which catches re-fetches whose result bytes differ (timestamps, ordering).
//!
//! Both skip the first serve and both suppress polling: repeats landing within
//! [`REREAD_ADJACENT_GAP`] message positions of the previous serve advance the
//! baseline without counting, so a long polling chain never accumulates waste.
//!
//! Waste-signal detection itself lives in [`crate::waste_signals`]; this module
//! only calls it and aggregates the result.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::OnceLock;

use md5::{Digest, Md5};
use regex::Regex;
use serde_json::Value;

use crate::tokenizer::Tokenizer;
use crate::waste_signals::{detect_waste_signals, WasteSignals};

/// Tool results below this size legitimately repeat ("ok", empty diffs, exit
/// codes) and are not evidence of a re-read.
pub const REREAD_MIN_TOKENS: usize = 50;

/// Repeats this close (in message positions) to the previous serve are polling,
/// not re-reads.
///
/// Consecutive tool turns sit 2 apart (the assistant `tool_use` message lies
/// between results); 3 also absorbs a thinking/user nudge in the loop. Larger
/// gaps mean the agent moved on and then came back — the over-compression
/// signal we want.
pub const REREAD_ADJACENT_GAP: usize = 3;

/// Message-shape overhead added to content and tool-result blocks.
const CONTENT_OVERHEAD: usize = 4;

/// Message-shape overhead added to tool-call blocks, which carry more envelope
/// (id, type, function wrapper) than a plain content block.
const TOOL_CALL_OVERHEAD: usize = 10;

/// Best-effort markers that a user message carries retrieved documents.
///
/// Kept as separate strings for readability; joined into one alternation by
/// [`rag_pattern`], exactly as Python builds `RAG_PATTERN`.
pub const RAG_MARKERS: [&str; 6] = [
    r"\[Document\s*\d+\]",
    r"\[Source:\s*",
    r"<context>",
    r"<document>",
    r"Retrieved from:",
    r"From the knowledge base:",
];

fn rag_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!("(?i){}", RAG_MARKERS.join("|"))).expect("rag pattern is valid")
    })
}

/// Canonical CCR retrieval-marker shapes.
///
/// Mirrors the alternation in the compression-units transform; kept local
/// because the parser is a base module.
fn ccr_retrieval_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"Retrieve more: hash=|Retrieve original: hash=|<<ccr:[^>]+>>")
            .expect("ccr marker pattern is valid")
    })
}

/// MD5 of `text`, truncated to 16 hex chars.
///
/// Byte-for-byte identical to Python's `hashlib.md5(text.encode()).hexdigest()[:16]`.
/// Used only to group identical text inside a single request — never persisted
/// and never compared across processes — but parity keeps cross-language
/// fixtures comparable.
pub fn compute_hash(text: &str) -> String {
    let digest = Md5::digest(text.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// True when `text` looks like RAG-injected content.
pub fn is_rag_content(text: &str) -> bool {
    rag_pattern().is_match(text)
}

/// What kind of context a [`Block`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BlockKind {
    System,
    User,
    Assistant,
    ToolCall,
    ToolResult,
    Rag,
    Unknown,
}

impl BlockKind {
    /// The Python `Literal` spelling, used as the breakdown key.
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::System => "system",
            BlockKind::User => "user",
            BlockKind::Assistant => "assistant",
            BlockKind::ToolCall => "tool_call",
            BlockKind::ToolResult => "tool_result",
            BlockKind::Rag => "rag",
            BlockKind::Unknown => "unknown",
        }
    }
}

/// Per-block metadata.
///
/// Python carries a free-form `dict[str, Any]` here, but only ever writes four
/// keys, so this is a struct. `tool_call_id` is `Option` rather than absent
/// because Python writes `flags["tool_call_id"] = message.get("tool_call_id")`
/// unconditionally for `role == "tool"` — present-but-`None` and absent are
/// indistinguishable to every reader.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockFlags {
    pub tool_call_id: Option<String>,
    pub function_name: Option<String>,
    /// Identity of a tool invocation: name plus arguments with JSON key order
    /// normalized. See [`canonical_call_key`].
    pub call_key: Option<String>,
    /// Set only when [`WasteSignals::total`] is above zero, matching Python.
    pub waste_signals: Option<WasteSignals>,
}

/// Atomic unit of context analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub text: String,
    pub tokens_est: usize,
    pub content_hash: String,
    /// Position in the original message list.
    pub source_index: usize,
    pub flags: BlockFlags,
}

// ---------------------------------------------------------------------------
// Python-compatible JSON and str() rendering
//
// Block text feeds hashes and token counts, so the exact bytes matter. Python
// builds these strings with `json.dumps` and f-strings; serde_json's defaults
// differ in two ways that would change every hash: it writes no space after
// `,`/`:`, and it emits non-ASCII literally where Python escapes it.
// ---------------------------------------------------------------------------

/// Render `value` the way `json.dumps` would.
///
/// `sort_keys` mirrors the keyword argument. `compact` picks the separator pair:
/// `(",", ":")` when true (Python's explicit `separators=`), otherwise Python's
/// default `(", ", ": ")`. Non-ASCII is escaped, matching `ensure_ascii=True`.
fn json_dumps(value: &Value, sort_keys: bool, compact: bool) -> String {
    let mut out = String::new();
    write_json(&mut out, value, sort_keys, compact);
    out
}

fn write_json(out: &mut String, value: &Value, sort_keys: bool, compact: bool) {
    let (item_sep, key_sep) = if compact { (",", ":") } else { (", ", ": ") };
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_json_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                write_json(out, item, sort_keys, compact);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            if sort_keys {
                keys.sort();
            }
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                write_json_string(out, key);
                out.push_str(key_sep);
                write_json(out, &map[key], sort_keys, compact);
            }
            out.push('}');
        }
    }
}

fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if c.is_ascii() => out.push(c),
            // ensure_ascii=True: astral characters become a surrogate pair.
            c => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

/// Render `value` the way Python's `str()` would inside an f-string.
///
/// A top-level string is emitted bare; nested strings get `repr()` quoting.
/// This only matters for malformed payloads (a dict where a string was
/// expected); well-formed traffic never reaches the container arms.
fn python_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => python_repr(other),
    }
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if n.as_i64().is_none() && n.as_u64().is_none() && f.fract() == 0.0 {
                    return format!("{f:.1}");
                }
            }
            n.to_string()
        }
        Value::String(s) => python_repr_str(s),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(python_repr).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", python_repr_str(k), python_repr(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

fn python_repr_str(s: &str) -> String {
    // Python prefers single quotes and switches to double quotes only when the
    // string contains a single quote but no double quote.
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python truthiness for a JSON value, as used by `if content:`.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key).filter(|v| !v.is_null())
}

fn get_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

// ---------------------------------------------------------------------------
// Tool-call and tool-result helpers
// ---------------------------------------------------------------------------

/// Normalize a single tool call into the canonical OpenAI dict shape.
///
/// Python also has to cope with provider SDK objects (Pydantic models with
/// attribute access and no `.get()`) reaching this function from streaming
/// integrations. Rust only ever sees `serde_json::Value`, so that whole branch
/// collapses: an object passes through, anything else degrades to an empty
/// object rather than erroring — over-compressing a malformed tool call is far
/// cheaper than failing the request.
fn coerce_tool_call_to_dict(tc: &Value) -> Value {
    if tc.is_object() {
        tc.clone()
    } else {
        Value::Object(serde_json::Map::new())
    }
}

/// Canonical identity for a tool invocation: name plus arguments with JSON key
/// order normalized, so semantically identical calls hash equal even when the
/// provider serializes arguments differently.
pub fn canonical_call_key(name: &str, arguments: &Value) -> String {
    // A string payload is re-parsed first, so `{"a":1,"b":2}` and
    // `{"b": 2, "a": 1}` land on the same key.
    let parsed;
    let mut args = arguments;
    if let Value::String(s) = arguments {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            parsed = v;
            args = &parsed;
        }
    }
    let canon = match args {
        Value::Object(_) | Value::Array(_) => json_dumps(args, true, true),
        other => python_str(other),
    };
    compute_hash(&format!("{name}\u{0}{canon}"))
}

/// Extract text from a tool-result payload.
///
/// Handles the Anthropic `tool_result` block (`content` is a plain string or a
/// list of `{"type": "text"}` blocks) and the Strands/Bedrock `toolResult`
/// payload (items keyed `{"text": ...}` or `{"json": ...}` with no `type`
/// field). Non-text inner blocks such as images are skipped.
fn extract_tool_result_text(payload: &Value) -> String {
    let Some(inner) = get(payload, "content") else {
        return String::new();
    };
    match inner {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut pieces: Vec<String> = Vec::new();
            for item in items {
                match item {
                    Value::Object(map) => {
                        if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                            pieces.push(get_str(item, "text").unwrap_or_default());
                        } else if !map.contains_key("type") {
                            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                pieces.push(text.to_string());
                            } else if let Some(json) = map.get("json") {
                                pieces.push(json_dumps(json, false, false));
                            }
                        }
                    }
                    Value::String(s) => pieces.push(s.clone()),
                    _ => {}
                }
            }
            pieces.join("\n")
        }
        Value::Object(_) => json_dumps(inner, false, false),
        other => python_str(other),
    }
}

// ---------------------------------------------------------------------------
// Per-message parsing
// ---------------------------------------------------------------------------

/// Parse a single message into blocks.
///
/// Usually one block, but a message can carry several: a text container plus
/// dedicated blocks for each nested `tool_result` / `tool_use` part, plus one
/// per OpenAI `tool_calls` entry.
pub fn parse_message_to_blocks(
    message: &Value,
    index: usize,
    tokenizer: &dyn Tokenizer,
) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let role = message
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("unknown");

    if let Some(content) = message.get("content").filter(|c| is_truthy(c)) {
        let mut tool_result_parts: Vec<&Value> = Vec::new();
        let mut tool_use_parts: Vec<&Value> = Vec::new();

        let text = match content {
            Value::String(s) => s.clone(),
            Value::Array(parts) => {
                let mut text_parts: Vec<String> = Vec::new();
                for part in parts {
                    match part {
                        Value::Object(map) => {
                            let part_type = part.get("type").and_then(|t| t.as_str());
                            if part_type == Some("text") {
                                text_parts.push(get_str(part, "text").unwrap_or_default());
                            } else if part_type == Some("tool_result") {
                                // Anthropic Messages format nests tool output
                                // one level deeper.
                                tool_result_parts.push(part);
                            } else if map.contains_key("toolResult") {
                                // Strands/Bedrock converse format.
                                tool_result_parts.push(part);
                            } else if part_type == Some("tool_use") {
                                tool_use_parts.push(part);
                            } else if map.contains_key("toolUse") {
                                tool_use_parts.push(part);
                            }
                        }
                        Value::String(s) => text_parts.push(s.clone()),
                        _ => {}
                    }
                }
                text_parts.join("\n")
            }
            other => python_str(other),
        };

        let kind = match role {
            "system" => BlockKind::System,
            "user" => {
                if is_rag_content(&text) {
                    BlockKind::Rag
                } else {
                    BlockKind::User
                }
            }
            "assistant" => BlockKind::Assistant,
            "tool" => BlockKind::ToolResult,
            _ => BlockKind::Unknown,
        };

        let mut flags = BlockFlags::default();
        if role == "tool" {
            flags.tool_call_id = get_str(message, "tool_call_id");
        }
        let waste = detect_waste_signals(&text, tokenizer);
        if waste.total() > 0 {
            flags.waste_signals = Some(waste);
        }

        let mut tr_blocks: Vec<Block> = Vec::new();
        for part in &tool_result_parts {
            let nested = part.get("toolResult");
            let payload = nested.unwrap_or(part);
            if !payload.is_object() {
                continue;
            }
            let tr_text = extract_tool_result_text(payload);
            if tr_text.is_empty() {
                continue;
            }
            let tr_id = if nested.is_some() {
                get_str(payload, "toolUseId")
            } else {
                get_str(part, "tool_use_id")
            };
            let mut tr_flags = BlockFlags {
                tool_call_id: tr_id,
                ..Default::default()
            };
            let tr_waste = detect_waste_signals(&tr_text, tokenizer);
            if tr_waste.total() > 0 {
                tr_flags.waste_signals = Some(tr_waste);
            }
            tr_blocks.push(Block {
                kind: BlockKind::ToolResult,
                tokens_est: tokenizer.count_text(&tr_text) + CONTENT_OVERHEAD,
                content_hash: compute_hash(&tr_text),
                text: tr_text,
                source_index: index,
                flags: tr_flags,
            });
        }

        // A tool-result-only message is fully represented by its dedicated
        // blocks; the empty container block would only add overhead tokens
        // nothing points at.
        if !text.is_empty() || tr_blocks.is_empty() {
            blocks.push(Block {
                kind,
                tokens_est: tokenizer.count_text(&text) + CONTENT_OVERHEAD,
                content_hash: compute_hash(&text),
                text,
                source_index: index,
                flags,
            });
        }
        blocks.extend(tr_blocks);

        for part in &tool_use_parts {
            let nested = part.get("toolUse");
            let payload = nested.unwrap_or(part);
            if !payload.is_object() {
                continue;
            }
            let tu_name = payload
                .get("name")
                .filter(|n| is_truthy(n))
                .and_then(|n| n.as_str())
                .unwrap_or("unknown")
                .to_string();
            let empty = Value::Object(serde_json::Map::new());
            let tu_args = payload.get("input").unwrap_or(&empty);
            let tu_id = if nested.is_some() {
                get_str(payload, "toolUseId")
            } else {
                get_str(payload, "id")
            };
            let tu_text = format!("{tu_name}({})", json_dumps(tu_args, true, false));
            let call_key = canonical_call_key(&tu_name, tu_args);
            blocks.push(Block {
                kind: BlockKind::ToolCall,
                tokens_est: tokenizer.count_text(&tu_text) + TOOL_CALL_OVERHEAD,
                content_hash: compute_hash(&tu_text),
                source_index: index,
                flags: BlockFlags {
                    tool_call_id: tu_id,
                    function_name: Some(tu_name),
                    call_key: Some(call_key),
                    waste_signals: None,
                },
                text: tu_text,
            });
        }
    }

    // OpenAI format: assistant messages carry `tool_calls` alongside content.
    if let Some(Value::Array(tool_calls)) = message.get("tool_calls").filter(|v| is_truthy(v)) {
        for raw_tc in tool_calls {
            let tc = coerce_tool_call_to_dict(raw_tc);
            let empty = Value::Object(serde_json::Map::new());
            let func = tc.get("function").unwrap_or(&empty);
            // Python uses `.get(key, default)`: the default applies only when
            // the key is absent, so an explicit null renders as "None".
            let name_text = func
                .get("name")
                .map(python_str)
                .unwrap_or_else(|| "unknown".to_string());
            let args_value = func.get("arguments").cloned().unwrap_or(Value::String(
                // Absent arguments default to the empty string.
                String::new(),
            ));
            let tc_text = format!("{name_text}({})", python_str(&args_value));
            let function_name = func
                .get("name")
                .and_then(|n| n.as_str())
                .map(str::to_string);
            let key_name = func
                .get("name")
                .filter(|n| is_truthy(n))
                .and_then(|n| n.as_str())
                .unwrap_or("unknown")
                .to_string();
            blocks.push(Block {
                kind: BlockKind::ToolCall,
                tokens_est: tokenizer.count_text(&tc_text) + TOOL_CALL_OVERHEAD,
                content_hash: compute_hash(&tc_text),
                source_index: index,
                flags: BlockFlags {
                    tool_call_id: get_str(&tc, "id"),
                    function_name,
                    call_key: Some(canonical_call_key(&key_name, &args_value)),
                    waste_signals: None,
                },
                text: tc_text,
            });
        }
    }

    // Neither content nor tool calls: keep a placeholder so message positions
    // stay addressable and the envelope cost is still accounted for.
    if blocks.is_empty() {
        blocks.push(Block {
            kind: BlockKind::Unknown,
            text: String::new(),
            tokens_est: CONTENT_OVERHEAD,
            content_hash: compute_hash(""),
            source_index: index,
            flags: BlockFlags::default(),
        });
    }

    blocks
}

// ---------------------------------------------------------------------------
// Whole-request parsing
// ---------------------------------------------------------------------------

/// Parse all messages into blocks, a per-kind token breakdown, and the
/// request-level waste signals.
///
/// `compressed_messages` is an optional post-transform copy of the same list.
/// When supplied and the same length, re-reads whose first serve was replaced
/// by a CCR retrieval marker are additionally attributed to
/// [`WasteSignals::reread_compressed_tokens`]: the model never saw the full
/// first serve, so the repeat is over-compression rather than agent behavior.
/// Lossless reshaping (no marker) is deliberately not attributed.
pub fn parse_messages(
    messages: &[Value],
    tokenizer: &dyn Tokenizer,
    compressed_messages: Option<&[Value]>,
) -> (Vec<Block>, BTreeMap<String, i64>, WasteSignals) {
    let mut all_blocks: Vec<Block> = Vec::new();
    let mut total_waste = WasteSignals::default();

    for (i, msg) in messages.iter().enumerate() {
        let blocks = parse_message_to_blocks(msg, i, tokenizer);
        for block in &blocks {
            if let Some(ws) = block.flags.waste_signals {
                total_waste.json_bloat_tokens += ws.json_bloat_tokens;
                total_waste.html_noise_tokens += ws.html_noise_tokens;
                total_waste.base64_tokens += ws.base64_tokens;
                total_waste.whitespace_tokens += ws.whitespace_tokens;
                total_waste.dynamic_date_tokens += ws.dynamic_date_tokens;
                total_waste.repetition_tokens += ws.repetition_tokens;
            }
        }
        all_blocks.extend(blocks);
    }

    // --- Pass 1: identical tool-result content served twice. ---
    let mut counted_results: HashSet<usize> = HashSet::new();
    let mut group_order: Vec<String> = Vec::new();
    let mut reread_groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, block) in all_blocks.iter().enumerate() {
        if block.kind == BlockKind::ToolResult && block.tokens_est >= REREAD_MIN_TOKENS {
            reread_groups
                .entry(block.content_hash.clone())
                .or_insert_with(|| {
                    group_order.push(block.content_hash.clone());
                    Vec::new()
                })
                .push(idx);
        }
    }
    let attribute = compressed_messages.is_some_and(|c| c.len() == messages.len());

    for hash in &group_order {
        let group = &reread_groups[hash];
        // The message that first served the content is the original; only
        // copies in *later* messages are re-reads. Duplicates inside the
        // original message are excluded, and so are polling repeats.
        let mut prev_index = all_blocks[group[0]].source_index;
        let mut counted_tokens = 0usize;
        let mut newly_counted: Vec<usize> = Vec::new();
        for &idx in group {
            let block = &all_blocks[idx];
            if block.source_index == prev_index {
                continue;
            }
            let is_polling = block.source_index - prev_index <= REREAD_ADJACENT_GAP;
            prev_index = block.source_index;
            if !is_polling {
                counted_tokens += block.tokens_est;
                newly_counted.push(idx);
            }
        }
        if counted_tokens == 0 {
            continue;
        }
        total_waste.reread_tokens += counted_tokens;
        counted_results.extend(newly_counted);

        if let Some(compressed) = compressed_messages.filter(|_| attribute) {
            let first = &all_blocks[group[0]];
            let transformed = parse_message_to_blocks(
                &compressed[first.source_index],
                first.source_index,
                tokenizer,
            );
            let transformed_text: String = transformed
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if ccr_retrieval_marker_re().is_match(&transformed_text)
                && !transformed_text.contains(&first.text)
            {
                total_waste.reread_compressed_tokens += counted_tokens;
            }
        }
    }

    // --- Pass 2: the same call re-issued, whose result bytes may differ. ---
    // Timestamps, mtimes and ordering defeat the content-hash pass, but the
    // agent asking the same question twice is still a re-fetch. Results the
    // first pass already counted are skipped so nothing is billed twice.
    let mut results_by_call_id: HashMap<&str, usize> = HashMap::new();
    for (idx, block) in all_blocks.iter().enumerate() {
        if block.kind == BlockKind::ToolResult {
            if let Some(tc_id) = block
                .flags
                .tool_call_id
                .as_deref()
                .filter(|s| !s.is_empty())
            {
                results_by_call_id.entry(tc_id).or_insert(idx);
            }
        }
    }

    let mut call_order: Vec<String> = Vec::new();
    let mut call_groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, block) in all_blocks.iter().enumerate() {
        if block.kind == BlockKind::ToolCall {
            if let Some(key) = block.flags.call_key.as_deref().filter(|s| !s.is_empty()) {
                call_groups
                    .entry(key.to_string())
                    .or_insert_with(|| {
                        call_order.push(key.to_string());
                        Vec::new()
                    })
                    .push(idx);
            }
        }
    }

    for key in &call_order {
        let group = &call_groups[key];
        let mut prev_index = all_blocks[group[0]].source_index;
        for &idx in group {
            let block = &all_blocks[idx];
            if block.source_index == prev_index {
                continue;
            }
            let is_polling = block.source_index - prev_index <= REREAD_ADJACENT_GAP;
            prev_index = block.source_index;
            if is_polling {
                continue;
            }
            let Some(result_idx) = block
                .flags
                .tool_call_id
                .as_deref()
                .and_then(|id| results_by_call_id.get(id).copied())
            else {
                continue;
            };
            let result = &all_blocks[result_idx];
            if result.tokens_est < REREAD_MIN_TOKENS || counted_results.contains(&result_idx) {
                continue;
            }
            total_waste.reread_tokens += result.tokens_est;
            counted_results.insert(result_idx);
        }
    }

    let mut breakdown: BTreeMap<String, i64> = BTreeMap::new();
    for block in &all_blocks {
        *breakdown
            .entry(block.kind.as_str().to_string())
            .or_insert(0) += block.tokens_est as i64;
    }

    (all_blocks, breakdown, total_waste)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Counts whitespace-separated words, so expectations stay readable. The
    /// Python side of every measurement below used the same rule.
    #[derive(Debug)]
    struct WordTokenizer;

    impl Tokenizer for WordTokenizer {
        fn count_text(&self, text: &str) -> usize {
            text.split_whitespace().count()
        }

        fn backend(&self) -> crate::tokenizer::Backend {
            crate::tokenizer::Backend::Estimation
        }
    }

    fn parse_one(message: Value) -> Vec<Block> {
        parse_message_to_blocks(&message, 0, &WordTokenizer)
    }

    fn parse_all(messages: Vec<Value>) -> (Vec<Block>, BTreeMap<String, i64>, WasteSignals) {
        parse_messages(&messages, &WordTokenizer, None)
    }

    /// Measured against `hashlib.md5(b"hello world").hexdigest()[:16]`.
    #[test]
    fn compute_hash_matches_python_md5() {
        assert_eq!(compute_hash("hello world"), "5eb63bbbe01eeed0");
        assert_eq!(compute_hash(""), "d41d8cd98f00b204");
    }

    #[test]
    fn string_content_makes_one_block() {
        let blocks = parse_one(json!({"role": "user", "content": "hello there friend"}));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::User);
        assert_eq!(blocks[0].text, "hello there friend");
        // 3 words + 4 overhead.
        assert_eq!(blocks[0].tokens_est, 7);
        assert_eq!(blocks[0].content_hash, compute_hash("hello there friend"));
    }

    #[test]
    fn roles_map_to_kinds() {
        for (role, kind) in [
            ("system", BlockKind::System),
            ("assistant", BlockKind::Assistant),
            ("tool", BlockKind::ToolResult),
            ("developer", BlockKind::Unknown),
        ] {
            let blocks = parse_one(json!({"role": role, "content": "x"}));
            assert_eq!(blocks[0].kind, kind, "role {role}");
        }
    }

    #[test]
    fn multimodal_text_parts_join_with_newlines() {
        let blocks = parse_one(json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "alpha beta"},
                {"type": "image", "source": {"data": "..."}},
                {"type": "text", "text": "gamma"},
            ],
        }));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "alpha beta\ngamma");
        assert_eq!(blocks[0].tokens_est, 7);
    }

    #[test]
    fn anthropic_tool_result_becomes_its_own_block() {
        let blocks = parse_one(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "call_1",
                "content": [{"type": "text", "text": "one two three"}],
            }],
        }));
        // Tool-result-only message: the empty container block is skipped.
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::ToolResult);
        assert_eq!(blocks[0].text, "one two three");
        assert_eq!(blocks[0].tokens_est, 7);
        assert_eq!(blocks[0].flags.tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn a_tool_result_beside_text_keeps_the_container_block() {
        let blocks = parse_one(json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "here you go"},
                {"type": "tool_result", "tool_use_id": "c1", "content": "one two three"},
            ],
        }));
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, BlockKind::User);
        assert_eq!(blocks[0].text, "here you go");
        assert_eq!(blocks[1].kind, BlockKind::ToolResult);
        assert_eq!(blocks[1].text, "one two three");
    }

    #[test]
    fn bedrock_tool_result_is_unwrapped() {
        let blocks = parse_one(json!({
            "role": "user",
            "content": [{
                "toolResult": {
                    "toolUseId": "tu_9",
                    "content": [{"text": "alpha beta"}, {"json": {"b": 2, "a": 1}}],
                },
            }],
        }));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::ToolResult);
        // Untyped items use their `text`; `json` items are dumped with
        // Python's default separators (space after `:` and `,`).
        assert_eq!(blocks[0].text, "alpha beta\n{\"b\": 2, \"a\": 1}");
        assert_eq!(blocks[0].flags.tool_call_id.as_deref(), Some("tu_9"));
    }

    #[test]
    fn anthropic_tool_use_becomes_a_tool_call_block() {
        let blocks = parse_one(json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "tu_1",
                "name": "read_file",
                "input": {"path": "/tmp/a", "limit": 10},
            }],
        }));
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, BlockKind::Assistant);
        assert_eq!(blocks[0].text, "");
        assert_eq!(blocks[0].tokens_est, 4);
        assert_eq!(blocks[1].kind, BlockKind::ToolCall);
        // sort_keys=True, default separators.
        assert_eq!(
            blocks[1].text,
            "read_file({\"limit\": 10, \"path\": \"/tmp/a\"})"
        );
        // 4 whitespace-separated words + 10 overhead.
        assert_eq!(blocks[1].tokens_est, 14);
        assert_eq!(blocks[1].content_hash, "c4e011757b24743b");
        assert_eq!(blocks[1].flags.function_name.as_deref(), Some("read_file"));
        assert_eq!(blocks[1].flags.tool_call_id.as_deref(), Some("tu_1"));
    }

    #[test]
    fn bedrock_tool_use_is_unwrapped() {
        let blocks = parse_one(json!({
            "role": "assistant",
            "content": [{"toolUse": {"toolUseId": "tu_2", "name": "ls", "input": {"p": "."}}}],
        }));
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1].kind, BlockKind::ToolCall);
        assert_eq!(blocks[1].text, "ls({\"p\": \".\"})");
        assert_eq!(blocks[1].flags.tool_call_id.as_deref(), Some("tu_2"));
    }

    #[test]
    fn openai_tool_calls_become_tool_call_blocks() {
        let blocks = parse_one(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_a",
                "type": "function",
                "function": {"name": "search", "arguments": "{\"q\": \"rust\"}"},
            }],
        }));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::ToolCall);
        assert_eq!(blocks[0].text, "search({\"q\": \"rust\"})");
        assert_eq!(blocks[0].tokens_est, 12);
        assert_eq!(blocks[0].flags.tool_call_id.as_deref(), Some("call_a"));
        assert_eq!(blocks[0].flags.function_name.as_deref(), Some("search"));
    }

    /// The whole point of the canonical key: the same call serialized with a
    /// different key order has to hash equal.
    #[test]
    fn call_key_ignores_argument_key_order() {
        let a = canonical_call_key("f", &json!("{\"a\": 1, \"b\": 2}"));
        let b = canonical_call_key("f", &json!("{\"b\": 2, \"a\": 1}"));
        assert_eq!(a, b);
        assert_ne!(a, canonical_call_key("g", &json!("{\"a\": 1, \"b\": 2}")));
    }

    #[test]
    fn an_empty_message_gets_a_minimal_block() {
        let blocks = parse_one(json!({"role": "assistant"}));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Unknown);
        assert_eq!(blocks[0].text, "");
        assert_eq!(blocks[0].tokens_est, 4);
        assert_eq!(blocks[0].content_hash, compute_hash(""));
    }

    #[test]
    fn rag_markers_reclassify_a_user_message() {
        let blocks = parse_one(json!({"role": "user", "content": "[Document 3] the answer"}));
        assert_eq!(blocks[0].kind, BlockKind::Rag);
        assert!(is_rag_content("retrieved from: somewhere"));
        assert!(is_rag_content("<CONTEXT>"));
        assert!(!is_rag_content("plain question"));
    }

    /// A user message that only mentions documents in passing stays `user`.
    #[test]
    fn plain_user_text_is_not_rag() {
        let blocks = parse_one(json!({"role": "user", "content": "read the document please"}));
        assert_eq!(blocks[0].kind, BlockKind::User);
    }

    #[test]
    fn breakdown_sums_tokens_per_kind() {
        let (_, breakdown, _) = parse_all(vec![
            json!({"role": "system", "content": "a b c"}),
            json!({"role": "user", "content": "d e"}),
            json!({"role": "assistant", "content": "f"}),
        ]);
        assert_eq!(breakdown.get("system"), Some(&7));
        assert_eq!(breakdown.get("user"), Some(&6));
        assert_eq!(breakdown.get("assistant"), Some(&5));
    }

    /// Builds a tool result of `n` words, which under `WordTokenizer` is
    /// `n + 4` tokens — comfortably over `REREAD_MIN_TOKENS`.
    fn big_result(tool_use_id: &str, words: usize) -> Value {
        let text = (0..words)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": text}],
        })
    }

    fn filler(text: &str) -> Value {
        json!({"role": "assistant", "content": text})
    }

    #[test]
    fn a_distant_repeat_of_the_same_result_counts_as_a_reread() {
        let messages = vec![
            big_result("a", 60), // index 0
            filler("thinking"),  // 1
            filler("thinking"),  // 2
            filler("thinking"),  // 3
            filler("thinking"),  // 4
            big_result("b", 60), // 5 — 5 positions later
        ];
        let (_, _, waste) = parse_all(messages);
        assert_eq!(waste.reread_tokens, 64);
        assert_eq!(waste.total(), 64);
        assert_eq!(waste.reread_compressed_tokens, 0);
    }

    /// Repeats within `REREAD_ADJACENT_GAP` are polling, and a chain of them
    /// must never accumulate: each advances the baseline instead of counting.
    #[test]
    fn adjacent_repeats_are_polling_and_do_not_count() {
        let messages = vec![
            big_result("a", 60), // 0
            filler("x"),         // 1
            big_result("b", 60), // 2 — gap 2
            filler("x"),         // 3
            big_result("c", 60), // 4 — gap 2
            filler("x"),         // 5
            big_result("d", 60), // 6 — gap 2
        ];
        let (_, _, waste) = parse_all(messages);
        assert_eq!(waste.reread_tokens, 0);
    }

    /// Exactly `REREAD_ADJACENT_GAP` apart is still polling; one further is not.
    #[test]
    fn the_polling_gap_boundary_is_inclusive() {
        let at_gap = vec![
            big_result("a", 60),
            filler("x"),
            filler("x"),
            big_result("b", 60), // gap 3
        ];
        assert_eq!(parse_all(at_gap).2.reread_tokens, 0);

        let past_gap = vec![
            big_result("a", 60),
            filler("x"),
            filler("x"),
            filler("x"),
            big_result("b", 60), // gap 4
        ];
        assert_eq!(parse_all(past_gap).2.reread_tokens, 64);
    }

    /// Small results legitimately repeat (`ok`, empty diffs) and are below the
    /// size floor.
    #[test]
    fn small_repeated_results_are_below_the_floor() {
        let messages = vec![
            big_result("a", 5),
            filler("x"),
            filler("x"),
            filler("x"),
            filler("x"),
            big_result("b", 5),
        ];
        assert_eq!(parse_all(messages).2.reread_tokens, 0);
    }

    /// When the transformed copy of the first serve carries a CCR marker and
    /// the original text is gone, the re-read is attributable to compression.
    #[test]
    fn ccr_attribution_marks_compressed_first_serves() {
        let messages = vec![
            big_result("a", 60),
            filler("x"),
            filler("x"),
            filler("x"),
            big_result("b", 60),
        ];
        let mut compressed = messages.clone();
        compressed[0] = json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "a",
                "content": "[compressed] Retrieve more: hash=abc123",
            }],
        });
        let (_, _, waste) = parse_messages(&messages, &WordTokenizer, Some(&compressed));
        assert_eq!(waste.reread_tokens, 64);
        assert_eq!(waste.reread_compressed_tokens, 64);
        // The compressed subset is excluded from the total to avoid
        // double-counting.
        assert_eq!(waste.total(), 64);
    }

    /// Lossless reshaping leaves no marker, so the re-read stays agent
    /// behavior and is not attributed to compression.
    #[test]
    fn lossless_transforms_are_not_attributed() {
        let messages = vec![
            big_result("a", 60),
            filler("x"),
            filler("x"),
            filler("x"),
            big_result("b", 60),
        ];
        let compressed = messages.clone();
        let (_, _, waste) = parse_messages(&messages, &WordTokenizer, Some(&compressed));
        assert_eq!(waste.reread_tokens, 64);
        assert_eq!(waste.reread_compressed_tokens, 0);
    }

    /// A length mismatch means the two lists are not the same request, so
    /// attribution is skipped entirely.
    #[test]
    fn attribution_is_skipped_when_lengths_differ() {
        let messages = vec![
            big_result("a", 60),
            filler("x"),
            filler("x"),
            filler("x"),
            big_result("b", 60),
        ];
        let compressed = vec![json!({
            "role": "user",
            "content": "Retrieve more: hash=abc123",
        })];
        let (_, _, waste) = parse_messages(&messages, &WordTokenizer, Some(&compressed));
        assert_eq!(waste.reread_tokens, 64);
        assert_eq!(waste.reread_compressed_tokens, 0);
    }

    /// The same call re-issued far enough apart counts even though the result
    /// bytes differ, which the content-hash pass cannot see.
    #[test]
    fn a_reissued_call_counts_when_the_result_bytes_differ() {
        let call = |id: &str| {
            json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use", "id": id, "name": "git_status", "input": {"repo": "."},
                }],
            })
        };
        let result = |id: &str, salt: usize| {
            let text = (0..60)
                .map(|i| format!("w{i}_{salt}"))
                .collect::<Vec<_>>()
                .join(" ");
            json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": id, "content": text}],
            })
        };
        let messages = vec![
            call("c1"),      // 0
            result("c1", 1), // 1
            filler("x"),     // 2
            filler("x"),     // 3
            filler("x"),     // 4
            call("c2"),      // 5 — gap 5 from the first call
            result("c2", 2), // 6
        ];
        let (_, _, waste) = parse_all(messages);
        // Different bytes, so pass 1 finds nothing; pass 2 bills the repeat's
        // result: 60 words + 4 overhead.
        assert_eq!(waste.reread_tokens, 64);
    }

    /// Identical content already billed by the content-hash pass must not be
    /// billed a second time by the re-issued-call pass.
    #[test]
    fn identical_repeats_are_never_billed_twice() {
        let call = |id: &str| {
            json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use", "id": id, "name": "git_status", "input": {"repo": "."},
                }],
            })
        };
        let messages = vec![
            call("c1"),
            big_result("c1", 60),
            filler("x"),
            filler("x"),
            filler("x"),
            call("c2"),
            big_result("c2", 60),
        ];
        let (_, _, waste) = parse_all(messages);
        assert_eq!(waste.reread_tokens, 64);
    }

    #[test]
    fn waste_signals_accumulate_across_messages() {
        let blob = "A".repeat(80);
        let (blocks, _, waste) = parse_all(vec![
            json!({"role": "user", "content": format!("data {blob}==")}),
            json!({"role": "user", "content": format!("more {blob}==")}),
        ]);
        assert_eq!(waste.base64_tokens, 2);
        assert!(blocks[0].flags.waste_signals.is_some());
    }

    /// Python emits `ensure_ascii=True`, so non-ASCII arguments are escaped
    /// before hashing. Getting this wrong would change every hash.
    #[test]
    fn json_dumps_escapes_non_ascii() {
        // Measured from `json.dumps`: an astral character becomes a surrogate
        // pair, not a single escape.
        assert_eq!(json_dumps(&json!("café"), false, false), r#""caf\u00e9""#);
        assert_eq!(json_dumps(&json!("😀"), false, false), r#""\ud83d\ude00""#);
    }

    #[test]
    fn json_dumps_honours_separators_and_sorting() {
        let v = json!({"b": 1, "a": [1, 2]});
        assert_eq!(json_dumps(&v, true, false), r#"{"a": [1, 2], "b": 1}"#);
        assert_eq!(json_dumps(&v, true, true), r#"{"a":[1,2],"b":1}"#);
        assert_eq!(json_dumps(&v, false, true), r#"{"b":1,"a":[1,2]}"#);
    }
}
