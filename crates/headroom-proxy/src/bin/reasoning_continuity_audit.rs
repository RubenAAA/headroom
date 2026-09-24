//! Reasoning-continuity audit (§6 of the harness doc).
//!
//! Offline-only. Joins inbound captures (`req-*.json`) with their wire
//! copies (`out/<request_id>.json`, paired by `request_id`) and checks the
//! invariants `compression/prior_thinking.rs` promises:
//!
//! A. last-assistant thinking intact: the last assistant message's
//!    thinking/redacted_thinking multiset is byte-identical inbound vs
//!    outbound (it must stay while a tool loop may still be open).
//! B. no fabricated reasoning: every outbound thinking block exists inbound.
//! C. monotonic strip per session: a thinking hash stripped once (inbound
//!    present, outbound absent) never reappears in a later turn's outbound.
//!    (Replay carries stripped bytes forward; resurgence means the store
//!    and the gate disagree.)
//! D. kept-block fidelity: redacted_thinking survivors are byte-identical,
//!    and kept `thinking` blocks retain their `signature` (Anthropic 400s
//!    a signed block without one).
//!
//! This proposes zero new dropping — it proves continuity holds. Counts,
//! not savings: per `rejected/prior-thinking-billing-question.md` wider
//! dropping saves ~$0, so any violation here is a correctness bug, not a
//! missed saving.
//!
//! Usage:
//!   cargo run -p headroom-proxy --bin reasoning_continuity_audit -- <capture_dir>

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};

use serde_json::Value;

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

fn is_thinking(b: &Value) -> bool {
    matches!(
        b.get("type").and_then(|t| t.as_str()),
        Some("thinking") | Some("redacted_thinking")
    )
}

fn block_hash(b: &Value) -> u64 {
    hash_bytes(&serde_json::to_vec(b).unwrap_or_default())
}

/// Thinking-block hashes per assistant message, in order. `None` content
/// (string messages) contributes no entry.
fn thinking_per_message(body: &Value) -> Vec<Vec<(u64, Value)>> {
    let empty = vec![];
    let msgs = body
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty);
    let mut out = Vec::new();
    for msg in msgs {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let mut blocks = Vec::new();
        if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
            for b in arr {
                if is_thinking(b) {
                    blocks.push((block_hash(b), b.clone()));
                }
            }
        }
        out.push(blocks);
    }
    out
}

fn multiset(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

struct Turn {
    ts_ms: u64,
    seq: u64,
    request_id: String,
    session_key: String,
    model: String,
    inbound: Value,
    outbound: Value,
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: reasoning_continuity_audit <capture_dir>");
        std::process::exit(2);
    });

    // Load inbound envelopes.
    struct Inbound {
        ts_ms: u64,
        seq: u64,
        session_key: String,
        model: String,
        body: Value,
    }
    let mut inbound: HashMap<String, Inbound> = HashMap::new();
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
            continue;
        }
        let body = env.get("body").cloned().unwrap_or(Value::Null);
        if body.is_null() {
            continue;
        }
        let rid = env
            .get("request_id")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        if rid.is_empty() {
            continue;
        }
        inbound.insert(
            rid,
            Inbound {
                ts_ms: env.get("ts_ms").and_then(|s| s.as_u64()).unwrap_or(0),
                seq: env.get("seq").and_then(|s| s.as_u64()).unwrap_or(0),
                session_key: env
                    .get("session_key")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                model: body
                    .get("model")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                body,
            },
        );
    }

    // Join with outbound wire copies.
    let out_dir = std::path::Path::new(&dir).join("out");
    let mut turns: Vec<Turn> = Vec::new();
    let mut unpaired = 0usize;
    let out_entries = std::fs::read_dir(&out_dir).unwrap_or_else(|_| {
        eprintln!("no out/ dir in {dir} — nothing to audit (need wire copies)");
        std::process::exit(1);
    });
    for entry in out_entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let rid = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let Some(ib) = inbound.remove(&rid) else {
            unpaired += 1;
            continue;
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let outbound: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => continue,
        };
        turns.push(Turn {
            ts_ms: ib.ts_ms,
            seq: ib.seq,
            request_id: rid,
            session_key: ib.session_key.clone(),
            model: ib.model.clone(),
            inbound: ib.body,
            outbound,
        });
    }
    // Group by envelope (session_key, model): the same identity the proxy's
    // observer uses, so resurgence is tested within a true lineage.
    let mut sessions: BTreeMap<(String, String), Vec<Turn>> = BTreeMap::new();
    for t in turns {
        sessions
            .entry((t.session_key.clone(), t.model.clone()))
            .or_default()
            .push(t);
    }
    for v in sessions.values_mut() {
        v.sort_by_key(|t| (t.ts_ms, t.seq));
    }

    println!(
        "Auditing {} paired turns across {} session(s) (unpaired out/ files: {unpaired})",
        sessions.values().map(|v| v.len()).sum::<usize>(),
        sessions.len()
    );

    let mut paired_total = 0usize;
    let mut last_intact = 0usize;
    let mut fabricated = 0usize;
    let mut resurged = 0usize;
    let mut kept = 0usize;
    let mut kept_missing_sig = 0usize;
    let mut redacted_kept = 0usize;
    let mut violations: Vec<String> = Vec::new();

    for turns in sessions.values() {
        // Hashes stripped in this session so far (check C).
        let mut stripped: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        for t in turns {
            paired_total += 1;
            let in_msgs = thinking_per_message(&t.inbound);
            let out_msgs = thinking_per_message(&t.outbound);

            // A: last assistant message intact.
            let in_last = in_msgs.last().cloned().unwrap_or_default();
            let out_last = out_msgs.last().cloned().unwrap_or_default();
            let in_hashes: Vec<u64> = in_last.iter().map(|(h, _)| *h).collect();
            let out_hashes: Vec<u64> = out_last.iter().map(|(h, _)| *h).collect();
            if multiset(in_hashes.clone()) == multiset(out_hashes.clone()) {
                last_intact += 1;
            } else {
                violations.push(format!(
                    "{} A:last-assistant-changed in={} out={}",
                    t.request_id,
                    in_hashes.len(),
                    out_hashes.len()
                ));
            }

            // Index outbound hashes for B/C/D.
            let in_all: std::collections::BTreeSet<u64> = in_msgs
                .iter()
                .flat_map(|m| m.iter().map(|(h, _)| *h))
                .collect();
            let out_all: std::collections::BTreeSet<u64> = out_msgs
                .iter()
                .flat_map(|m| m.iter().map(|(h, _)| *h))
                .collect();

            // B: fabricated reasoning.
            for h in out_all.iter() {
                if !in_all.contains(h) {
                    fabricated += 1;
                    violations.push(format!("{} B:fabricated-thinking {h:016x}", t.request_id));
                }
            }

            // C: resurgence of stripped hashes.
            for h in out_all.iter() {
                if stripped.contains(h) {
                    resurged += 1;
                    violations.push(format!(
                        "{} C:stripped-thinking-resurged {h:016x}",
                        t.request_id
                    ));
                }
            }
            // Record newly stripped hashes (skip the last assistant message:
            // its thinking is live, not stripped — check A covers it).
            let in_prior: std::collections::BTreeSet<u64> = in_msgs
                .iter()
                .take(in_msgs.len().saturating_sub(1))
                .flat_map(|m| m.iter().map(|(h, _)| *h))
                .collect();
            for h in in_prior {
                if !out_all.contains(&h) {
                    stripped.insert(h);
                }
            }

            // D: fidelity of kept blocks.
            for msg in out_msgs.iter() {
                for (h, b) in msg {
                    if in_all.contains(h) {
                        kept += 1;
                        if b.get("type").and_then(|t| t.as_str()) == Some("thinking")
                            && b.get("signature").is_none()
                        {
                            kept_missing_sig += 1;
                        }
                        if b.get("type").and_then(|t| t.as_str()) == Some("redacted_thinking") {
                            redacted_kept += 1;
                        }
                    }
                }
            }
        }
    }

    println!("\n== continuity ==");
    println!("paired turns:              {paired_total}");
    println!(
        "A last-assistant intact:   {last_intact} ({:.1}%)",
        pct(last_intact, paired_total)
    );
    println!("B fabricated blocks:       {fabricated}");
    println!("C resurgences:             {resurged}");
    println!("D kept blocks:             {kept} (missing signature: {kept_missing_sig}, redacted kept: {redacted_kept})");
    println!("\n== violations ({}) ==", violations.len());
    for v in violations.iter().take(20) {
        println!("  {v}");
    }
    if violations.len() > 20 {
        println!("  ... and {} more", violations.len() - 20);
    }
    println!("\nNote: groups are envelope (session_key, model) — the same identity the");
    println!("observer uses. Cross-check against ledger conversation keys remains a follow-up.");
}

fn pct(a: usize, b: usize) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 / b as f64 * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(last_thinking: &str, prior_thinking: Option<&str>) -> Value {
        let mut msgs = vec![];
        if let Some(p) = prior_thinking {
            msgs.push(serde_json::json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": p, "signature": "sig"},
                {"type": "text", "text": "did a thing"}
            ]}));
            msgs.push(serde_json::json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
            ]}));
        }
        msgs.push(serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": last_thinking, "signature": "sig"},
            {"type": "text", "text": "next"}
        ]}));
        serde_json::json!({"model": "m", "messages": msgs})
    }

    #[test]
    fn intact_last_assistant_matches() {
        let b = body("plan", Some("old"));
        let in_last = thinking_per_message(&b).pop().unwrap();
        let out_last = thinking_per_message(&b).pop().unwrap();
        assert_eq!(
            multiset(in_last.iter().map(|(h, _)| *h).collect()),
            multiset(out_last.iter().map(|(h, _)| *h).collect())
        );
    }

    #[test]
    fn stripped_last_assistant_detected() {
        let ib = body("plan", Some("old"));
        let ob = serde_json::json!({"model": "m", "messages": [
            {"role": "assistant", "content": [{"type": "text", "text": "next"}]}
        ]});
        let in_last: Vec<u64> = thinking_per_message(&ib)
            .pop()
            .unwrap()
            .iter()
            .map(|(h, _)| *h)
            .collect();
        let out_last: Vec<u64> = thinking_per_message(&ob)
            .pop()
            .unwrap_or_default()
            .iter()
            .map(|(h, _)| *h)
            .collect();
        assert_ne!(multiset(in_last), multiset(out_last));
    }
}
