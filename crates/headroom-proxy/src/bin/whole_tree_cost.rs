//! Whole-tree cost attribution (§6 of the harness doc).
//!
//! Offline-only. Groups captures by envelope `session_key` (one tree per
//! session across all models), prices each turn with per-model pricing
//! ratios (stable run within the turn's own (session, model) lineage =
//! read, rest = write), and reports per-tree totals with a per-model split.
//! Guardrail instrument for routing proposals: ship only when tree cost
//! drops. See `harness-whole-tree-cost.md`.
//!
//! Honest approximations (all stated in the output):
//! - planner = first-turn model of the tree; workers = the rest. Role is
//!   not observable offline; per-model shares let the reader re-designate.
//! - session_key is the drift hash: system rewrites re-key, so re-keyed
//!   continuations file as separate trees (the `recache-rekey-floor.md`
//!   undercount). True key-stable joining needs the ledger's
//!   session_key_hash — follow-up, not this file.
//! - No sidecar filtering: tiny turns stay in their tree and show up as
//!   small-model shares.
//!
//! Usage:
//!   cargo run -p headroom-proxy --bin whole_tree_cost -- <capture_dir> [--write-tier 5m|1h]

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

fn body_tokens(body: &Value, tok: &dyn Tokenizer) -> usize {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    tok.count_text(&String::from_utf8_lossy(&bytes))
}

/// Leading-segment stability key: system + tools + per-message hashes.
fn segments(body: &Value) -> Vec<u64> {
    let mut segs = Vec::new();
    if let Some(v) = body.get("system") {
        segs.push(hash_bytes(&serde_json::to_vec(v).unwrap_or_default()));
    }
    if let Some(v) = body.get("tools") {
        segs.push(hash_bytes(&serde_json::to_vec(v).unwrap_or_default()));
    }
    let empty = vec![];
    for m in body
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty)
    {
        segs.push(hash_bytes(&serde_json::to_vec(m).unwrap_or_default()));
    }
    segs
}

fn rates(model: &str, write_tier_1h: bool) -> (f64, f64) {
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

struct Turn {
    ts_ms: u64,
    seq: u64,
    model: String,
    body: Value,
}

#[derive(Default)]
struct Tree {
    turns: usize,
    tokens: usize,
    equiv: f64,
    first_turns: usize,
    /// model -> (turns, equiv)
    models: BTreeMap<String, (usize, f64)>,
    planner: String,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| {
        eprintln!("usage: whole_tree_cost <capture_dir> [--write-tier 5m|1h]");
        std::process::exit(2);
    });
    let mut write_tier_1h = true;
    for a in args {
        if a == "--write-tier=5m" {
            write_tier_1h = false;
        } else if a == "--write-tier=1h" {
            write_tier_1h = true;
        } else {
            eprintln!("unknown arg {a:?} (want --write-tier=5m|1h)");
            std::process::exit(2);
        }
    }

    // session_key -> turns (all models = the tree).
    let mut trees: BTreeMap<String, Vec<Turn>> = BTreeMap::new();
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
        trees.entry(sk).or_default().push(Turn {
            ts_ms: env.get("ts_ms").and_then(|s| s.as_u64()).unwrap_or(0),
            seq: env.get("seq").and_then(|s| s.as_u64()).unwrap_or(0),
            model: body
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
                .to_string(),
            body,
        });
        files += 1;
    }
    for v in trees.values_mut() {
        v.sort_by_key(|t| (t.ts_ms, t.seq));
    }
    println!(
        "Loaded {files} turns across {} trees from {dir} (skipped {skipped})",
        trees.len()
    );
    if trees.is_empty() {
        std::process::exit(1);
    }

    let mut acc: Vec<(String, Tree)> = Vec::new();
    // (session, model) -> previous segment keys, for lineage-local stability.
    let mut prev: HashMap<(String, String), Vec<u64>> = HashMap::new();

    for (sk, turns) in &trees {
        let mut tree = Tree {
            planner: turns.first().map(|t| t.model.clone()).unwrap_or_default(),
            ..Tree::default()
        };
        for (idx, turn) in turns.iter().enumerate() {
            let tok = get_tokenizer(&turn.model);
            let tokens = body_tokens(&turn.body, tok.as_ref());
            let segs = segments(&turn.body);
            let key = (sk.clone(), turn.model.clone());
            let stable = match prev.get(&key) {
                None => 0,
                Some(p) => {
                    let mut n = 0;
                    for (cur, old) in segs.iter().zip(p.iter()) {
                        if cur == old {
                            n += 1;
                        } else {
                            break;
                        }
                    }
                    n
                }
            };
            prev.insert(key, segs);
            // Segment-count stability → token split needs per-segment tokens;
            // approximate: stable fraction of segments × tokens. Ranking-grade,
            // stated as such (exact split lives in section_cost_baseline).
            let frac = if turns.is_empty() {
                0.0
            } else {
                stable as f64 / segments(&turn.body).len().max(1) as f64
            };
            let _ = idx;
            let (rm, wm) = rates(&turn.model, write_tier_1h);
            let read_tok = tokens as f64 * frac;
            let write_tok = tokens as f64 * (1.0 - frac);
            let equiv = read_tok * rm + write_tok * wm;
            tree.turns += 1;
            tree.tokens += tokens;
            tree.equiv += equiv;
            if stable == 0 {
                tree.first_turns += 1;
            }
            let e = tree.models.entry(turn.model.clone()).or_default();
            e.0 += 1;
            e.1 += equiv;
        }
        acc.push((sk.clone(), tree));
    }
    acc.sort_by(|a, b| b.1.equiv.partial_cmp(&a.1.equiv).unwrap());

    println!(
        "\n== trees by cost (planner = first-turn model; write tier {}) ==",
        if write_tier_1h { "1h" } else { "5m" }
    );
    println!(
        "{:<14} {:>7} {:>12} {:>14} {:>12}  models(turns:share%)",
        "tree", "turns", "tokens", "billed_equiv", "planner%"
    );
    println!("{}", "-".repeat(110));
    for (sk, t) in acc.iter().take(30) {
        let short = sk.chars().take(12).collect::<String>();
        let planner_share = t
            .models
            .get(&t.planner)
            .map(|(_, e)| e / t.equiv * 100.0)
            .unwrap_or(0.0);
        let mut models: Vec<_> = t.models.iter().collect();
        models.sort_by(|a, b| b.1 .1.partial_cmp(&a.1 .1).unwrap());
        let mstr = models
            .iter()
            .take(4)
            .map(|(m, (n, e))| format!("{}:{n}:{:0.0}%", short_name(m), e / t.equiv * 100.0))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "{:<14} {:>7} {:>12} {:>14.0} {:>11.1}%  {mstr}",
            short, t.turns, t.tokens, t.equiv, planner_share
        );
    }

    // Summary: worker-share distribution.
    let n = acc.len().max(1);
    let mean_turns = acc.iter().map(|(_, t)| t.turns).sum::<usize>() as f64 / n as f64;
    let mean_worker: f64 = acc
        .iter()
        .map(|(_, t)| {
            let ps = t
                .models
                .get(&t.planner)
                .map(|(_, e)| e / t.equiv * 100.0)
                .unwrap_or(0.0);
            100.0 - ps
        })
        .sum::<f64>()
        / n as f64;
    let single_model = acc.iter().filter(|(_, t)| t.models.len() == 1).count();
    println!("\n== summary ==");
    println!("trees: {}  mean turns/tree: {:.1}", acc.len(), mean_turns);
    println!("single-model trees: {single_model} (no routing question there)");
    println!("mean non-planner (worker) cost share: {mean_worker:.1}%");
    println!("\nNotes: stability here is segment-count fraction (ranking-grade); exact");
    println!("token splits live in section_cost_baseline. Re-keyed continuations file");
    println!("as separate trees (undercount — needs the ledger session_key_hash join).");
    println!("No sidecar filtering; tiny turns show as small-model shares.");
}

fn short_name(m: &str) -> String {
    m.replace("claude-", "")
        .chars()
        .take(14)
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_cover_system_tools_messages() {
        let b: Value = serde_json::from_str(
            r#"{"system":"s","tools":[],"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .unwrap();
        assert_eq!(segments(&b).len(), 3);
    }

    #[test]
    fn unknown_model_falls_back() {
        assert_eq!(rates("nope-not-a-model", true), (0.10, 1.25));
    }

    #[test]
    fn short_name_trims() {
        assert_eq!(short_name("claude-opus-5"), "opus-5");
    }
}
