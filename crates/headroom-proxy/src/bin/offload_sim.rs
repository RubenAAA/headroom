//! Phase J PR-J0 — offline frozen-history offload simulator.
//!
//! Replays request bodies captured by `cache_stabilization::capture` and, for
//! every config in a sweep grid, computes the **relative** input-token cost of
//! a session under that offload policy — without touching the network or any
//! production code path. The output ranks configs so the J1+ build commits to
//! the winning combination on evidence (see
//! `REALIGNMENT/13-phase-J-history-offload.md` §11 "Phase J0").
//!
//! Usage:
//!   cargo run -p headroom-proxy --bin offload_sim -- <capture_dir>
//!
//! # Cost model (documented assumptions)
//!
//! The cacheable "hot zone" of an Anthropic request is the ordered segment list
//! `[system, tools, msg_0 .. msg_{frozen-1}]` (frozen floor from
//! `compute_frozen_count`). Anthropic prompt caching reads the longest prefix
//! that is byte-identical to the previous turn and (re)writes everything from
//! the first divergence onward. So per turn:
//!
//!   cost = 0.10 * stable_prefix_tokens          (cache read of the match)
//!        + 1.25 * (prefix_tokens - stable)       (cache write of the changed tail)
//!
//! Live-zone (post-frozen) tokens are identical across configs, so they are
//! excluded — we compare prefix economics only. Constants are Anthropic's
//! cache multipliers (read 0.10x, write 1.25x). Absolute numbers are
//! approximate; the *ranking* across configs is what J0 produces.

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use headroom_core::compute_frozen_count;
use headroom_core::tokenizer::{get_tokenizer, Tokenizer};
use serde_json::Value;

const CACHE_READ: f64 = 0.10;
const CACHE_WRITE: f64 = 1.25;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    /// Offload only when the client rebuilt system/tools this turn (§3).
    BoundaryOnly,
    /// Offload every N turns regardless of boundary.
    EveryN(usize),
    /// Offload whenever the (pre-offload) prefix exceeds this many tokens.
    SizeThreshold(usize),
    /// Boundary OR size threshold.
    Hybrid(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Depth {
    /// Add only the single largest eligible block per trigger.
    Largest1,
    /// Add up to K largest eligible blocks per trigger.
    TopK(usize),
    /// Add all eligible blocks.
    All,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    min_offload_tokens: usize,
    k_recent_turns: usize,
    trigger: Trigger,
    depth: Depth,
}

impl Config {
    fn label(&self) -> String {
        let trig = match self.trigger {
            Trigger::BoundaryOnly => "boundary".to_string(),
            Trigger::EveryN(n) => format!("every{n}"),
            Trigger::SizeThreshold(s) => format!("size{}k", s / 1000),
            Trigger::Hybrid(s) => format!("hybrid{}k", s / 1000),
        };
        let depth = match self.depth {
            Depth::Largest1 => "largest1".to_string(),
            Depth::TopK(k) => format!("top{k}"),
            Depth::All => "all".to_string(),
        };
        format!(
            "min{}/k{}/{}/{}",
            self.min_offload_tokens, self.k_recent_turns, trig, depth
        )
    }
}

/// One captured request envelope.
struct Turn {
    seq: u64,
    body: Value,
}

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

/// A prefix segment: its identity (byte hash) and token weight.
#[derive(Clone, Copy)]
struct Segment {
    key: u64,
    tokens: usize,
}

/// Per-turn extracted hot-zone view (client/original — pre-offload).
struct HotZone {
    system_key: u64,
    tools_key: u64,
    /// One entry per frozen message: (byte-hash, tokens, eligible-block info).
    messages: Vec<MsgInfo>,
}

#[derive(Clone)]
struct MsgInfo {
    /// Stable identity of the *original* message content (for offloaded-set keys).
    content_key: u64,
    full_tokens: usize,
    /// Tokens if this message's tool_result body is replaced by a marker.
    offloaded_tokens: usize,
    /// True if this message carries a tool_result block (offload candidate).
    has_tool_result: bool,
}

fn extract_hot_zone(body: &Value, tok: &dyn Tokenizer, marker_tokens: usize) -> HotZone {
    let canon = |v: &Value| -> (u64, usize) {
        let bytes = serde_json::to_vec(v).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        (hash_bytes(&bytes), tok.count_text(&text))
    };

    let (system_key, _) = body
        .get("system")
        .map(canon)
        .unwrap_or((hash_bytes(b""), 0));
    let (tools_key, _) = body.get("tools").map(canon).unwrap_or((hash_bytes(b""), 0));

    let frozen = compute_frozen_count(body);
    let empty = vec![];
    let msgs = body
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty);

    let mut messages = Vec::new();
    for msg in msgs.iter().take(frozen) {
        let (content_key, full_tokens) = canon(msg);
        let has_tool_result = msg
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
            })
            .unwrap_or(false);
        // Offloaded message ~= its non-tool_result scaffold + one marker.
        // Approximate as max(marker_tokens, 10% of full) so large blobs shrink
        // hard but tiny ones don't pretend to shrink below the marker cost.
        let offloaded_tokens = marker_tokens.max(full_tokens / 10);
        messages.push(MsgInfo {
            content_key,
            full_tokens,
            offloaded_tokens,
            has_tool_result,
        });
    }

    HotZone {
        system_key,
        tools_key,
        // store system/tools tokens by faking them into messages? No — keep
        // separate via closure recompute below.
        messages,
    }
}

/// Recompute system/tools token weights (kept out of HotZone to avoid a second
/// struct; cheap to redo once per turn).
fn prefix_fixed_tokens(body: &Value, tok: &dyn Tokenizer) -> (usize, usize) {
    let count = |v: Option<&Value>| -> usize {
        v.map(|x| {
            let bytes = serde_json::to_vec(x).unwrap_or_default();
            tok.count_text(&String::from_utf8_lossy(&bytes))
        })
        .unwrap_or(0)
    };
    (count(body.get("system")), count(body.get("tools")))
}

/// Result of simulating one session under one config.
#[derive(Default, Clone, Copy)]
struct SessionResult {
    cost: f64,
    write_tokens: f64,
    retrievals: usize,
}

fn simulate(
    turns: &[Turn],
    cfg: &Config,
    tok: &dyn Tokenizer,
    marker_tokens: usize,
) -> SessionResult {
    // Monotonic offloaded set: original message content_key once offloaded
    // stays offloaded (§7 invariant I3).
    let mut offloaded: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut prev_segments: Option<Vec<Segment>> = None;
    let mut prev_sys = u64::MAX;
    let mut prev_tools = u64::MAX;
    let mut out = SessionResult::default();

    for (idx, turn) in turns.iter().enumerate() {
        let hz = extract_hot_zone(&turn.body, tok, marker_tokens);
        let (sys_tokens, tools_tokens) = prefix_fixed_tokens(&turn.body, tok);

        // Boundary = client rebuilt system or tools vs previous turn.
        let boundary = idx > 0 && (hz.system_key != prev_sys || hz.tools_key != prev_tools);
        prev_sys = hz.system_key;
        prev_tools = hz.tools_key;

        let prefix_tokens_preoffload: usize =
            sys_tokens + tools_tokens + hz.messages.iter().map(|m| m.full_tokens).sum::<usize>();

        // Decide whether to EXTEND the offloaded set this turn.
        let should_extend = match cfg.trigger {
            Trigger::BoundaryOnly => boundary,
            Trigger::EveryN(n) => n > 0 && idx % n == 0,
            Trigger::SizeThreshold(s) => prefix_tokens_preoffload > s,
            Trigger::Hybrid(s) => boundary || prefix_tokens_preoffload > s,
        };

        if should_extend {
            // Protect the most recent k_recent turns (~2 messages/turn).
            let protect = cfg.k_recent_turns.saturating_mul(2);
            let cutoff = hz.messages.len().saturating_sub(protect);
            let mut candidates: Vec<(usize, &MsgInfo)> = hz
                .messages
                .iter()
                .enumerate()
                .filter(|(i, m)| {
                    *i < cutoff
                        && m.has_tool_result
                        && m.full_tokens >= cfg.min_offload_tokens
                        && !offloaded.contains(&m.content_key)
                })
                .collect();
            // Largest first.
            candidates.sort_by(|a, b| b.1.full_tokens.cmp(&a.1.full_tokens));
            let take = match cfg.depth {
                Depth::Largest1 => 1,
                Depth::TopK(k) => k,
                Depth::All => candidates.len(),
            };
            for (_, m) in candidates.into_iter().take(take) {
                offloaded.insert(m.content_key);
            }
        }

        // Build this turn's segment list AFTER applying the offloaded set.
        let mut segments = Vec::with_capacity(hz.messages.len() + 2);
        segments.push(Segment {
            key: hz.system_key,
            tokens: sys_tokens,
        });
        segments.push(Segment {
            key: hz.tools_key,
            tokens: tools_tokens,
        });
        for m in &hz.messages {
            if offloaded.contains(&m.content_key) {
                // Offloaded segment: deterministic marker → stable key derived
                // from the original content key (same input ⇒ same marker).
                segments.push(Segment {
                    key: m.content_key ^ 0xC0FF_EE00_DEAD_BEEF,
                    tokens: m.offloaded_tokens,
                });
            } else {
                segments.push(Segment {
                    key: m.content_key,
                    tokens: m.full_tokens,
                });
            }
        }

        // Longest byte-identical prefix vs previous turn (cache read region).
        let stable_tokens: usize = match &prev_segments {
            None => 0,
            Some(prev) => {
                let mut acc = 0;
                for (cur, old) in segments.iter().zip(prev.iter()) {
                    if cur.key == old.key {
                        acc += cur.tokens;
                    } else {
                        break;
                    }
                }
                acc
            }
        };
        let total: usize = segments.iter().map(|s| s.tokens).sum();
        let write = total.saturating_sub(stable_tokens);

        out.cost += CACHE_READ * stable_tokens as f64 + CACHE_WRITE * write as f64;
        out.write_tokens += write as f64;
        prev_segments = Some(segments);
    }

    out.retrievals = offloaded.len();
    out
}

fn sweep_grid() -> Vec<Config> {
    let mut v = Vec::new();
    for &min in &[256usize, 512, 1024, 2048] {
        for &k in &[2usize, 3, 5, 8] {
            for trigger in [
                Trigger::BoundaryOnly,
                Trigger::EveryN(3),
                Trigger::SizeThreshold(8000),
                Trigger::Hybrid(8000),
            ] {
                for depth in [Depth::Largest1, Depth::TopK(3), Depth::All] {
                    v.push(Config {
                        min_offload_tokens: min,
                        k_recent_turns: k,
                        trigger,
                        depth,
                    });
                }
            }
        }
    }
    v
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: offload_sim <capture_dir>");
        std::process::exit(2);
    });

    // Load envelopes, group by session, order by seq.
    let mut sessions: BTreeMap<String, Vec<Turn>> = BTreeMap::new();
    let mut files = 0usize;
    for entry in std::fs::read_dir(&dir).expect("read capture dir").flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
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
            continue; // v1 simulator is Anthropic-only.
        }
        let sk = env
            .get("session_key")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string();
        let seq = env.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
        let body = env.get("body").cloned().unwrap_or(Value::Null);
        sessions.entry(sk).or_default().push(Turn { seq, body });
        files += 1;
    }
    for turns in sessions.values_mut() {
        turns.sort_by_key(|t| t.seq);
    }

    println!(
        "Loaded {files} anthropic turns across {} session(s) from {dir}\n",
        sessions.len()
    );
    if sessions.is_empty() {
        eprintln!("no anthropic capture files found — run a session with HEADROOM_CAPTURE_DIR set");
        std::process::exit(1);
    }

    // Marker token cost: tokenize a representative marker once with a generic
    // tokenizer (model-agnostic enough for a constant).
    let probe_tok = get_tokenizer("claude-3-5-sonnet-20241022");
    let marker_tokens = probe_tok.count_text("<<ccr:0123456789ab,tool_result,4.2KB>>");

    let configs = sweep_grid();

    // Baseline: offload disabled (min so high nothing qualifies).
    let baseline_cfg = Config {
        min_offload_tokens: usize::MAX,
        k_recent_turns: 0,
        trigger: Trigger::BoundaryOnly,
        depth: Depth::Largest1,
    };

    // Aggregate each config across all sessions.
    let mut baseline_total = 0.0f64;
    let mut baseline_writes = 0.0f64;
    let mut agg: Vec<(Config, f64, f64, usize)> = Vec::new(); // (cfg, cost, writes, retrievals)

    for (sk, turns) in &sessions {
        let model = turns
            .first()
            .and_then(|t| t.body.get("model"))
            .and_then(|m| m.as_str())
            .unwrap_or("claude-3-5-sonnet-20241022");
        let tok = get_tokenizer(model);

        let base = simulate(turns, &baseline_cfg, tok.as_ref(), marker_tokens);
        baseline_total += base.cost;
        baseline_writes += base.write_tokens;
        eprintln!(
            "  session {}: {} turns, baseline prefix-cost {:.0}",
            &sk[..sk.len().min(12)],
            turns.len(),
            base.cost
        );

        for (i, cfg) in configs.iter().enumerate() {
            let r = simulate(turns, cfg, tok.as_ref(), marker_tokens);
            if let Some(slot) = agg.get_mut(i) {
                slot.1 += r.cost;
                slot.2 += r.write_tokens;
                slot.3 += r.retrievals;
            } else {
                agg.push((*cfg, r.cost, r.write_tokens, r.retrievals));
            }
        }
    }

    // Rank by savings vs baseline.
    let mut ranked: Vec<_> = agg
        .iter()
        .map(|(cfg, cost, writes, retr)| {
            let savings = if baseline_total > 0.0 {
                (baseline_total - cost) / baseline_total * 100.0
            } else {
                0.0
            };
            let write_delta = writes - baseline_writes;
            (cfg, savings, write_delta, *retr)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    println!("Baseline prefix-cost (relative units): {baseline_total:.0}");
    println!(
        "Baseline cache-write tokens: {baseline_writes:.0}\n\n\
         {:<34} {:>9} {:>14} {:>11}",
        "config", "savings%", "write_delta", "retrievals"
    );
    println!("{}", "-".repeat(70));
    let show = |row: &(&Config, f64, f64, usize)| {
        println!(
            "{:<34} {:>8.1}% {:>14.0} {:>11}",
            row.0.label(),
            row.1,
            row.2,
            row.3
        );
    };
    println!("== top 20 ==");
    for row in ranked.iter().take(20) {
        show(row);
    }
    println!("\n== best per trigger family ==");
    for fam in ["boundary", "every", "size", "hybrid"] {
        if let Some(row) = ranked.iter().find(|r| r.0.label().contains(fam)) {
            show(row);
        }
    }
    println!("\n== worst 5 (sanity: should show cache-thrash configs going negative) ==");
    for row in ranked.iter().rev().take(5) {
        show(row);
    }
    println!(
        "\nNote: write_delta < 0 means the config writes FEWER cache tokens than \
         baseline (rides the free boundary write, §3). write_delta >> 0 = thrash."
    );
}
