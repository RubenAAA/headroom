//! Offline replay of a captured corpus through the **real** offload transform.
//!
//! `offload_sim` models offload with its own cost arithmetic. This binary does
//! the opposite: it calls `offload_anthropic_request` itself, turn by turn,
//! with the same gate and the same drift-detector boundary the live proxy
//! uses, and reports the counters the proxy only ever emits as log lines. That
//! makes the offload picture checkable against a corpus instead of only
//! against a running session.
//!
//! Usage:
//!   cargo run --release -p headroom-proxy --bin offload_replay -- <capture_dir>
//!     [--min-bytes N] [--stale-margin N] [--stale-window N]

use std::collections::BTreeMap;

use headroom_proxy::cache_stabilization::drift_detector::{
    compute_structural_hash, observe_drift, ApiKind, DriftState,
};
use headroom_proxy::compression::ctx_offload::{
    offload_anthropic_request, CtxOffloadConfig, OffloadGate, OffloadPolicy,
};
use serde_json::Value;

/// Same LRU capacity the proxy gives both the drift detector and the gate.
const CAPACITY: usize = 1000;

struct Turn {
    seq: u64,
    body: Value,
}

#[derive(Default)]
struct Totals {
    turns: usize,
    turns_with_offload: usize,
    blocks_offloaded: usize,
    blocks_deferred: usize,
    window_offloads: usize,
    frozen_new_offloads: usize,
    tokens_saved: i64,
    boundaries: usize,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut dir = String::new();
    // Defaults are the flags the live proxy runs with, not clap's own defaults
    // (50_000 / 0 / 0) — those disable the near-tail band entirely, so a replay
    // under them says nothing about the counters this binary exists to explain.
    let mut min_bytes = 2_000usize;
    let mut stale_margin = 4usize;
    let mut stale_window = 4usize;
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or_else(|| {
                    eprintln!("expected a number after {arg}");
                    std::process::exit(2)
                })
        };
        match arg.as_str() {
            "--min-bytes" => min_bytes = value(),
            "--stale-margin" => stale_margin = value(),
            "--stale-window" => stale_window = value(),
            other => dir = other.to_string(),
        }
    }
    if dir.is_empty() {
        eprintln!(
            "usage: offload_replay <capture_dir> [--min-bytes N] \
             [--stale-margin N] [--stale-window N]"
        );
        std::process::exit(2);
    }

    let mut sessions: BTreeMap<String, Vec<Turn>> = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).expect("read capture dir").flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(envelope) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if envelope.get("endpoint").and_then(Value::as_str) != Some("anthropic") {
            continue;
        }
        let session = envelope
            .get("session_key")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let seq = envelope.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let body = envelope.get("body").cloned().unwrap_or(Value::Null);
        sessions
            .entry(session)
            .or_default()
            .push(Turn { seq, body });
    }
    for turns in sessions.values_mut() {
        turns.sort_by_key(|t| t.seq);
    }
    if sessions.is_empty() {
        eprintln!("no anthropic capture files found in {dir}");
        std::process::exit(1);
    }

    let config = CtxOffloadConfig {
        min_bytes,
        stale_margin,
        stale_window,
    };
    // The gate keys its offloaded-hash sets by session, so one instance covers
    // every session — the same instance the proxy shares across requests. Built
    // without persistence so the replay never reads or writes the live gate
    // directory.
    let gate = OffloadGate::new(CAPACITY);
    let drift = DriftState::new(CAPACITY);
    let mut totals = Totals::default();

    for (session_key, turns) in &sessions {
        for turn in turns {
            let mut body = turn.body.clone();
            // Same order as the proxy: hash the inbound body, ask the drift
            // detector whether this turn rebuilds the hot zone, then offload.
            let hash = compute_structural_hash(&body, ApiKind::Anthropic);
            let rebuild_boundary = observe_drift(&drift, session_key, hash).is_some();
            let policy = OffloadPolicy {
                gate: &gate,
                session_key,
                rebuild_boundary,
            };
            let out = offload_anthropic_request(&mut body, &config, Some(&policy));

            totals.turns += 1;
            totals.boundaries += usize::from(rebuild_boundary);
            totals.turns_with_offload += usize::from(out.blocks_offloaded > 0);
            totals.blocks_offloaded += out.blocks_offloaded;
            totals.blocks_deferred += out.blocks_deferred;
            totals.window_offloads += out.window_offloads;
            totals.frozen_new_offloads += out.frozen_new_offloads;
            totals.tokens_saved += out.tokens_saved;
        }
    }

    let per_turn = |n: usize| {
        if n == 0 {
            "n/a".to_string()
        } else {
            format!("1 per {:.1} turns", totals.turns as f64 / n as f64)
        }
    };
    println!(
        "offload_replay — real transform, min_bytes={min_bytes} \
         stale_margin={stale_margin} stale_window={stale_window}"
    );
    println!("rebuild_boundary from the live drift detector (not assumed)");
    println!("sessions            {:>10}", sessions.len());
    println!("turns               {:>10}", totals.turns);
    println!("boundary turns      {:>10}", totals.boundaries);
    println!("blocks_offloaded    {:>10}", totals.blocks_offloaded);
    println!("blocks_deferred     {:>10}", totals.blocks_deferred);
    println!(
        "window_offloads     {:>10}  ({})",
        totals.window_offloads,
        per_turn(totals.window_offloads)
    );
    println!("frozen_new_offloads {:>10}", totals.frozen_new_offloads);
    println!("tokens_saved        {:>10}", totals.tokens_saved);
    println!("turns with offload  {:>10}", totals.turns_with_offload);
}
