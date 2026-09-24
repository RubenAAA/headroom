//! Section-cost baseline (§1 of the harness doc).
//!
//! Offline-only. Reads request bodies captured by
//! `cache_stabilization::capture` (`HEADROOM_CAPTURE_DIR`) and reports the
//! three tables `LOOK_AT_THIS_WHEN_YOU_HAVE_TIME.md` §1 demands:
//!
//! 1. cost share by source × billing type (tokens + billed equivalents),
//! 2. static tokens/request, turns/session, stable-prefix share, first-turn share,
//! 3. per-tool call share + result tokens + `is_error` rate.
//!
//! Billing model: per turn, the longest byte-identical leading segment run
//! (`system`, `tools`, `msg_0..`) vs the previous turn of the same
//! `(session_key, model)` group counts as cache read, the rest as cache
//! write; first turns (and model switches) are all write and reported
//! separately per `recache-counting-rules.md` / `first-turn-write-share.md`.
//! Conversion uses `headroom-core::pricing::lookup` ratios (read/input,
//! write/input at the `--write-tier`, default `1h`); unknown models fall
//! back to 0.10/1.25. Ranking across sections is the product, not dollars.
//!
//! Usage:
//!   cargo run -p headroom-proxy --bin section_cost_baseline -- <capture_dir> [--write-tier 5m|1h]
//!
//! Deliberately no log joining in v1: captures carry no usage, so this is
//! the wire-shape baseline. A `--ledger` join against `turn_cost_ledger`
//! lines is the follow-up, not this file.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};

use headroom_core::pricing;
use headroom_core::tokenizer::{get_tokenizer, Tokenizer};
use serde_json::Value;

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

fn tok_count(tok: &dyn Tokenizer, v: &Value) -> usize {
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    tok.count_text(&String::from_utf8_lossy(&bytes))
}

/// Section labels. Coarse on purpose: the ranking question is "which leg",
/// not "which tool".
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
enum Section {
    System,
    ToolsBuiltin,
    ToolsMcp,
    UserText,
    AssistantText,
    Thinking,
    ToolUse,
    ResultFile,
    ResultSearch,
    ResultCommand,
    ResultEdit,
    ResultWeb,
    ResultAgent,
    ResultMeta,
    ResultMcp,
    ResultOther,
    ResultUnknown,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::System => "system",
            Section::ToolsBuiltin => "tools:builtin",
            Section::ToolsMcp => "tools:mcp",
            Section::UserText => "msg:user_text",
            Section::AssistantText => "msg:assistant_text",
            Section::Thinking => "msg:thinking",
            Section::ToolUse => "msg:tool_use",
            Section::ResultFile => "result:file",
            Section::ResultSearch => "result:search",
            Section::ResultCommand => "result:command",
            Section::ResultEdit => "result:edit_write",
            Section::ResultWeb => "result:web",
            Section::ResultAgent => "result:agent",
            Section::ResultMeta => "result:meta",
            Section::ResultMcp => "result:mcp",
            Section::ResultOther => "result:other",
            Section::ResultUnknown => "result:unknown_parent",
        }
    }
}

/// Parent tool name → result section. `mcp__server__fn` always wins as MCP
/// so per-server heaviness shows up in the right leg.
fn result_section(tool: &str) -> Section {
    let t = tool.to_lowercase();
    let base = t.trim_start_matches('_');
    if base.starts_with("mcp__") {
        return Section::ResultMcp;
    }
    match base {
        "read" | "view" | "read_file" => Section::ResultFile,
        "grep" | "glob" => Section::ResultSearch,
        "bash"
        | "bash_background"
        | "bash_background_output"
        | "bash_background_wait"
        | "bash_background_kill" => Section::ResultCommand,
        "edit" | "multiedit" | "write" | "apply_patch" => Section::ResultEdit,
        "webfetch" | "websearch" => Section::ResultWeb,
        "task" => Section::ResultAgent,
        "todowrite" | "todoread" | "question" | "skill" | "toolsearch" => Section::ResultMeta,
        _ => Section::ResultOther,
    }
}

fn is_mcp_tool(name: &str) -> bool {
    name.to_lowercase()
        .trim_start_matches('_')
        .starts_with("mcp__")
}

struct Segment {
    key: u64,
    section: Section,
    tokens: usize,
}

struct Turn {
    seq: u64,
    ts_ms: u64,
    request_id: String,
    model: String,
    body: Value,
}

fn tool_name_of(block: &Value) -> Option<String> {
    block
        .get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
}

/// Split one Anthropic body into labeled, tokenizer-weighted segments in
/// wire order: system, tools (builtin/mcp split by name), then per-message
/// blocks. Also returns per-tool call info for table 3.
fn segmentize(
    body: &Value,
    tok: &dyn Tokenizer,
    tool_calls: &mut HashMap<String, usize>,
) -> Vec<Segment> {
    let mut segs = Vec::new();

    if let Some(system) = body.get("system") {
        // `system` may be a string or an array of blocks; count as one segment.
        segs.push(Segment {
            key: hash_bytes(&serde_json::to_vec(system).unwrap_or_default()),
            section: Section::System,
            tokens: tok_count(tok, system),
        });
    }

    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let (mut builtin, mut mcp): (Vec<Value>, Vec<Value>) = (Vec::new(), Vec::new());
        for t in tools {
            let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if is_mcp_tool(name) {
                mcp.push(t.clone());
            } else {
                builtin.push(t.clone());
            }
        }
        // Two segments so the legs split; byte-hash keeps stability exact.
        let bv = Value::Array(builtin);
        let mv = Value::Array(mcp);
        segs.push(Segment {
            key: hash_bytes(&serde_json::to_vec(&bv).unwrap_or_default()),
            section: Section::ToolsBuiltin,
            tokens: tok_count(tok, &bv),
        });
        segs.push(Segment {
            key: hash_bytes(&serde_json::to_vec(&mv).unwrap_or_default()),
            section: Section::ToolsMcp,
            tokens: tok_count(tok, &mv),
        });
        for t in tools {
            if let Some(n) = tool_name_of(t) {
                *tool_calls.entry(n).or_insert(0) += 1;
            }
        }
    }

    let empty = vec![];
    let msgs = body
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty);

    for msg in msgs {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = msg.get("content");
        // String-content messages: single text segment.
        if let Some(s) = content.and_then(|c| c.as_str()) {
            let section = match role {
                "user" => Section::UserText,
                "assistant" => Section::AssistantText,
                _ => Section::UserText,
            };
            let bytes = format!("{role}:{s}");
            segs.push(Segment {
                key: hash_bytes(bytes.as_bytes()),
                section,
                tokens: tok.count_text(&bytes),
            });
            continue;
        }
        let blocks = content
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();

        // Map tool_use id → parent name for this message's own tool_use
        // blocks; tool_result blocks reference ids from earlier assistant
        // messages, resolved via the turn-wide map built below.
        let mut local_ids: HashMap<String, String> = HashMap::new();
        for b in &blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                if let (Some(id), Some(name)) = (
                    b.get("id").and_then(|i| i.as_str()),
                    b.get("name").and_then(|n| n.as_str()),
                ) {
                    local_ids.insert(id.to_string(), name.to_string());
                }
            }
        }

        // Turn-wide map is threaded through `tool_use_ids` by the caller;
        // here we only classify with what we can see, leaving unknown
        // parents to the second pass in `analyze_session`.
        for b in &blocks {
            let btype = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let bytes = serde_json::to_vec(b).unwrap_or_default();
            let tokens = tok.count_text(&String::from_utf8_lossy(&bytes));
            let section = match btype {
                "text" => match role {
                    "assistant" => Section::AssistantText,
                    _ => Section::UserText,
                },
                "thinking" | "redacted_thinking" => Section::Thinking,
                "tool_use" => Section::ToolUse,
                "tool_result" => {
                    let parent = b
                        .get("tool_use_id")
                        .and_then(|i| i.as_str())
                        .and_then(|id| local_ids.get(id))
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    if parent.is_empty() {
                        Section::ResultUnknown
                    } else {
                        result_section(parent)
                    }
                }
                _ => Section::UserText,
            };
            segs.push(Segment {
                key: hash_bytes(&bytes),
                section,
                tokens,
            });
        }
    }

    segs
}

#[derive(Default)]
struct ToolStats {
    turns_with_call: usize,
    result_tokens: usize,
    result_blocks: usize,
    error_blocks: usize,
}

fn is_error_result(block: &Value) -> bool {
    block
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

#[derive(Default)]
struct Accum {
    read_tokens: f64,
    write_tokens: f64,
    read_equiv: f64,
    write_equiv: f64,
}

fn rates(model: &str, write_tier_1h: bool) -> (f64, f64) {
    // Ratios to input. Unknown models fall back to Anthropic 0.10/1.25.
    if let Some(p) = pricing::lookup(model) {
        let input = p.input_rate(false).max(f64::EPSILON);
        let read = p.cache_read_rate(false).map(|r| r / input).unwrap_or(0.10);
        let write = if write_tier_1h {
            p.cache_write_1h_rate(false)
                .or(p.cache_write_rate(false))
                .map(|r| r / input)
        } else {
            p.cache_write_rate(false).map(|r| r / input)
        }
        .unwrap_or(1.25);
        (read, write)
    } else {
        (0.10, 1.25)
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| {
        eprintln!("usage: section_cost_baseline <capture_dir> [--write-tier 5m|1h]");
        std::process::exit(2);
    });
    let mut write_tier_1h = true;
    while let Some(a) = args.next() {
        if a == "--write-tier" {
            match args.next().as_deref() {
                Some("5m") => write_tier_1h = false,
                Some("1h") => write_tier_1h = true,
                other => {
                    eprintln!("--write-tier expects 5m|1h, got {other:?}");
                    std::process::exit(2);
                }
            }
        } else {
            eprintln!("unknown arg {a:?}");
            std::process::exit(2);
        }
    }

    // Load envelopes, group by (session_key, model), order by seq.
    // Model switches reset the stable prefix (separate cache lineage —
    // same reason usage_observer compares within one model).
    let mut sessions: BTreeMap<(String, String), Vec<Turn>> = BTreeMap::new();
    let mut files = 0usize;
    let mut skipped = 0usize;
    let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| {
        eprintln!("read capture dir {dir}: {e}");
        std::process::exit(1);
    });
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // Skip outbound `out/` wire copies: same request_id as an inbound
        // envelope, would double-count every turn.
        if path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some("out")
        {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let env: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if env.get("endpoint").and_then(|e| e.as_str()) != Some("anthropic") {
            skipped += 1;
            continue;
        }
        let body = env.get("body").cloned().unwrap_or(Value::Null);
        if body.is_null() {
            skipped += 1;
            continue;
        }
        let sk = env
            .get("session_key")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string();
        let model = body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();
        let seq = env.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
        // ts_ms breaks ties across runs: seq restarts at 0 per process, so
        // a dir holding several runs would interleave on seq alone.
        let ts_ms = env.get("ts_ms").and_then(|s| s.as_u64()).unwrap_or(0);
        let request_id = env
            .get("request_id")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        sessions.entry((sk, model.clone())).or_default().push(Turn {
            seq,
            ts_ms,
            request_id,
            model,
            body,
        });
        files += 1;
    }
    for turns in sessions.values_mut() {
        turns.sort_by_key(|t| (t.ts_ms, t.seq));
    }

    println!(
        "Loaded {files} anthropic turns across {} session(s) from {dir} (skipped {skipped} non-anthropic/empty)",
        sessions.len()
    );
    if sessions.is_empty() {
        eprintln!("no anthropic capture files found — run a session with HEADROOM_CAPTURE_DIR set");
        std::process::exit(1);
    }

    let mut by_section: BTreeMap<Section, Accum> = BTreeMap::new();
    let mut tools: BTreeMap<String, ToolStats> = BTreeMap::new();
    // Session ids per tool for "share of sessions calling at least once".
    let mut tool_sessions: HashMap<String, std::collections::BTreeSet<String>> = HashMap::new();

    let mut total_turns = 0usize;
    let mut first_turns = 0usize;
    let mut first_turn_write_tokens = 0f64;
    let mut total_write_tokens = 0f64;
    let mut total_stable_tokens = 0f64;
    let mut total_tokens = 0f64;
    let mut static_sum = 0usize;

    for ((sk, _model), turns) in &sessions {
        let mut prev: Option<Vec<(u64, usize)>> = None;
        // Turn-wide tool_use id → name map for resolving tool_result parents
        // across messages (assistant tool_use precedes the user tool_result).
        for (idx, turn) in turns.iter().enumerate() {
            let tok = get_tokenizer(&turn.model);
            let tok = tok.as_ref();
            // Build id map first so result parents resolve across messages.
            let mut id_map: HashMap<String, String> = HashMap::new();
            let empty = vec![];
            let msgs = turn
                .body
                .get("messages")
                .and_then(|m| m.as_array())
                .unwrap_or(&empty);
            for msg in msgs {
                for b in msg
                    .get("content")
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_default()
                {
                    if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let (Some(id), Some(name)) = (
                            b.get("id").and_then(|i| i.as_str()),
                            b.get("name").and_then(|n| n.as_str()),
                        ) {
                            id_map.insert(id.to_string(), name.to_string());
                        }
                    }
                }
            }

            let mut tool_calls: HashMap<String, usize> = HashMap::new();
            let mut segs = segmentize(&turn.body, tok, &mut tool_calls);
            // tool_calls records tool *definitions*; call share comes from
            // tool_use blocks below. Keep definitions out of the call table.
            let _ = tool_calls;

            // Resolve ResultUnknown where the parent tool_use lives in an
            // earlier message (segmentize only sees the local message).
            // Segment order is system?, tools×2, then blocks in order, so
            // walk messages/blocks with the same index math.
            // Tools actually invoked this turn (tool_use names), for
            // call-share counting after the walk.
            let mut used_names: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            {
                let prefix = (if turn.body.get("system").is_some() {
                    1
                } else {
                    0
                }) + (if turn.body.get("tools").is_some() {
                    2
                } else {
                    0
                });
                // String-content messages contribute 1 seg each and carry no
                // tool_result; array messages contribute len(blocks).
                let mut si = prefix;
                for msg in msgs {
                    if msg.get("content").and_then(|c| c.as_str()).is_some() {
                        si += 1;
                        continue;
                    }
                    let blocks = msg
                        .get("content")
                        .and_then(|c| c.as_array())
                        .cloned()
                        .unwrap_or_default();
                    for b in &blocks {
                        if si < segs.len()
                            && segs[si].section == Section::ResultUnknown
                            && b.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                        {
                            if let Some(parent) = b
                                .get("tool_use_id")
                                .and_then(|i| i.as_str())
                                .and_then(|id| id_map.get(id))
                            {
                                segs[si].section = result_section(parent);
                            }
                        }
                        // Count result stats + tool call stats per block.
                        if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                            let parent = b
                                .get("tool_use_id")
                                .and_then(|i| i.as_str())
                                .and_then(|id| id_map.get(id))
                                .cloned()
                                .unwrap_or_else(|| "unknown".to_string());
                            let st = tools.entry(parent.clone()).or_default();
                            st.result_blocks += 1;
                            st.result_tokens += segs.get(si).map(|s| s.tokens).unwrap_or(0);
                            if is_error_result(b) {
                                st.error_blocks += 1;
                            }
                        }
                        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            if let Some(name) = b.get("name").and_then(|n| n.as_str()) {
                                tools.entry(name.to_string()).or_default();
                                used_names.insert(name.to_string());
                            }
                        }
                        si += 1;
                    }
                }
            }

            // Per-tool call share: turns/sessions carrying a tool_use of that name.
            for name in &used_names {
                tools.entry(name.clone()).or_default().turns_with_call += 1;
                tool_sessions
                    .entry(name.clone())
                    .or_default()
                    .insert(sk.clone());
            }

            let (read_mult, write_mult) = rates(&turn.model, write_tier_1h);
            let keys: Vec<(u64, usize)> = segs.iter().map(|s| (s.key, s.tokens)).collect();
            let total: usize = segs.iter().map(|s| s.tokens).sum();
            let stable: usize = match &prev {
                None => 0,
                Some(p) => {
                    let mut acc = 0;
                    for (cur, old) in keys.iter().zip(p.iter()) {
                        if cur.0 == old.0 {
                            acc += cur.1;
                        } else {
                            break;
                        }
                    }
                    acc
                }
            };
            prev = Some(keys);

            total_turns += 1;
            total_tokens += total as f64;
            total_stable_tokens += stable as f64;
            let turn_static: usize = segs
                .iter()
                .filter(|s| {
                    matches!(
                        s.section,
                        Section::System | Section::ToolsBuiltin | Section::ToolsMcp
                    )
                })
                .map(|s| s.tokens)
                .sum();
            static_sum += turn_static;

            // Attribute stable run to read, remainder to write, in order.
            let mut stable_left = stable;
            let is_first = idx == 0;
            if is_first {
                first_turns += 1;
                first_turn_write_tokens += total as f64;
            }
            for s in &segs {
                let stable_here = stable_left.min(s.tokens);
                let write_here = s.tokens - stable_here;
                stable_left -= stable_here;
                let e = by_section.entry(s.section).or_default();
                e.read_tokens += stable_here as f64;
                e.write_tokens += write_here as f64;
                e.read_equiv += stable_here as f64 * read_mult;
                e.write_equiv += write_here as f64 * write_mult;
            }
            total_write_tokens += (total - stable) as f64;
            let _ = turn.request_id.clone();
        }
    }

    // Table 1: cost share by source × billing.
    println!(
        "\n== 1. cost share by source × billing (write tier {}) ==",
        if write_tier_1h { "1h" } else { "5m" }
    );
    println!(
        "{:<22} {:>12} {:>12} {:>14} {:>9}",
        "section", "read_tok", "write_tok", "billed_equiv", "share%"
    );
    println!("{}", "-".repeat(75));
    let grand_equiv: f64 = by_section
        .values()
        .map(|a| a.read_equiv + a.write_equiv)
        .sum();
    let mut rows: Vec<_> = by_section.iter().collect();
    rows.sort_by(|a, b| {
        (b.1.read_equiv + b.1.write_equiv)
            .partial_cmp(&(a.1.read_equiv + a.1.write_equiv))
            .unwrap()
    });
    for (sec, a) in rows {
        let equiv = a.read_equiv + a.write_equiv;
        let share = if grand_equiv > 0.0 {
            equiv / grand_equiv * 100.0
        } else {
            0.0
        };
        println!(
            "{:<22} {:>12.0} {:>12.0} {:>14.0} {:>8.1}%",
            sec.label(),
            a.read_tokens,
            a.write_tokens,
            equiv,
            share
        );
    }
    println!("{}", "-".repeat(75));
    println!(
        "{:<22} {:>12.0} {:>12.0} {:>14.0}",
        "TOTAL",
        by_section.values().map(|a| a.read_tokens).sum::<f64>(),
        by_section.values().map(|a| a.write_tokens).sum::<f64>(),
        grand_equiv
    );

    // Table 2: static tokens, turns, stability, first-turn share.
    let nonfirst = total_turns.saturating_sub(first_turns);
    println!("\n== 2. static tokens, turns, stability ==");
    println!("turns: {total_turns} across {} session(s)", sessions.len());
    println!(
        "mean static tokens/req (system+tools): {:.0}",
        static_sum as f64 / total_turns.max(1) as f64
    );
    println!(
        "stable-prefix share (excl. first turns): {:.1}% of non-first-turn tokens",
        if total_tokens - first_turn_write_tokens > 0.0 {
            (total_stable_tokens) / (total_tokens - first_turn_write_tokens) * 100.0
        } else {
            0.0
        }
    );
    println!(
        "first-turn write share: {:.0} of {:.0} write tokens ({:.1}%), over {first_turns} first turns",
        first_turn_write_tokens,
        total_write_tokens,
        if total_write_tokens > 0.0 {
            first_turn_write_tokens / total_write_tokens * 100.0
        } else {
            0.0
        }
    );
    let _ = nonfirst;

    // Table 3: per-tool call share + result cost + errors.
    println!("\n== 3. per-tool call share + result cost ==");
    println!(
        "{:<32} {:>8} {:>8} {:>12} {:>8}",
        "tool", "turns", "sess", "res_tok", "err%"
    );
    println!("{}", "-".repeat(72));
    let mut trows: Vec<_> = tools.iter().collect();
    trows.sort_by_key(|a| std::cmp::Reverse(a.1.result_tokens));
    for (name, st) in trows.iter().take(40) {
        let sess = tool_sessions.get(*name).map(|s| s.len()).unwrap_or(0);
        let err = if st.result_blocks > 0 {
            st.error_blocks as f64 / st.result_blocks as f64 * 100.0
        } else {
            0.0
        };
        println!(
            "{:<32} {:>8} {:>8} {:>12} {:>7.1}%",
            name, st.turns_with_call, sess, st.result_tokens, err
        );
    }
    println!("\nNotes: stable = longest byte-identical leading run vs prev turn of the same");
    println!("(session, model); model switches reset (separate cache lineage). First turns are");
    println!("all write and reported separately. Tokenizer is per-turn model; billed equivalents");
    println!("use pricing ratios at the chosen write tier. Captures carry no usage — booked-only");
    println!("caveats and TTL-downgrade effects need the --ledger join (follow-up).");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn mcp_names_route_to_mcp_legs() {
        assert_eq!(result_section("mcp__server__read"), Section::ResultMcp);
        assert_eq!(result_section("Read"), Section::ResultFile);
        assert_eq!(result_section("Bash"), Section::ResultCommand);
        assert!(is_mcp_tool("mcp__x__y"));
        assert!(!is_mcp_tool("read"));
    }

    #[test]
    fn segmentize_splits_system_tools_text() {
        let tok = get_tokenizer("claude-sonnet-5");
        let body = v(
            r#"{"model":"claude-sonnet-5","system":"hello","tools":[{"name":"Read","input_schema":{}}],
            "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
        );
        let mut calls = HashMap::new();
        let segs = segmentize(&body, tok.as_ref(), &mut calls);
        let labels: Vec<_> = segs.iter().map(|s| s.section).collect();
        assert!(labels.contains(&Section::System));
        assert!(labels.contains(&Section::ToolsBuiltin));
        assert!(labels.contains(&Section::UserText));
        assert_eq!(calls.get("Read"), Some(&1));
    }

    #[test]
    fn unknown_parent_classifies_later() {
        assert_eq!(result_section("whatever-new-tool"), Section::ResultOther);
    }
}
