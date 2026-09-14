//! Offline replay-hit-rate for the prefix-replay store.
//!
//! Drives the **real** hit machinery turn by turn — `previous_turn_for` for
//! chain continuity, `overlay_cached_prefix_reported` for the hit decision,
//! `begin_request`/`complete` for tracker state — over a recorded (or
//! synthetic) capture corpus, and reports the applied/total rate the proxy
//! otherwise only emits as log lines. The recache-safety gate ("replay-hit
//! rate unchanged on the recorded corpus") pins this rate: any derivation or
//! canonicalization change that moves it fails loudly instead of silently
//! rotating every stored session.
//!
//! What this deliberately does NOT run: breakpoint placement/trimming (they
//! reshape bytes after the hit decision, never the decision itself) and the
//! network (no upstream, no mock). Placement is covered by
//! `integration_prefix_replay.rs`; this binary covers the decision.
//!
//! Usage:
//!   cargo run -p headroom-proxy --bin prefix_replay_rate -- <corpus_dir>
//!     [--expected expected.json]
//!
//! Corpus layout is the capture format (`capture.rs`): one `req-*.json`
//! envelope per turn with `session_key`, `seq`, and `body` (whose
//! `messages` array is the compared unit). See
//! `tests/fixtures/replay_corpus/` for a synthetic corpus with a reasoned
//! golden.
//!
//! `complete()` tokens are synthesized (`read=0, write=0`): captures store no
//! usage, and the commit — not the counts — is what advances chain state for
//! the next turn. A cold (all-zero) confirmation floor lets content matching
//! decide, exactly as on a cache-cold live session.

use std::collections::BTreeMap;
use std::path::Path;

use headroom_proxy::cache_stabilization::prefix_replay::{
    is_side_errand, overlay_cached_prefix_reported, SessionReplayStore,
};
use serde_json::Value;

struct Turn {
    seq: u64,
    request_id: String,
    body: Value,
}

#[derive(Default, Debug)]
struct Rate {
    turns: usize,
    applied: usize,
    /// Hit decision declined before overlay ran (cold store, TTL, …).
    prefix_miss: BTreeMap<String, usize>,
    /// Overlay ran and declined, by `ReplaySkip::as_str`.
    replay_skip: BTreeMap<String, usize>,
}

impl Rate {
    fn hit_rate(&self) -> f64 {
        if self.turns == 0 {
            0.0
        } else {
            self.applied as f64 / self.turns as f64
        }
    }
}

/// Load capture envelopes grouped by session, turns ordered by `seq`.
/// Envelopes without a messages array are not turns and are ignored.
fn load_corpus(dir: &Path) -> BTreeMap<String, Vec<Turn>> {
    let mut sessions: BTreeMap<String, Vec<Turn>> = BTreeMap::new();
    for entry in std::fs::read_dir(dir).expect("read corpus dir").flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // `expected.json` lives beside the envelopes but is a golden, not a turn.
        if path.file_name().and_then(|n| n.to_str()) == Some("expected.json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(envelope) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let body = envelope.get("body").cloned().unwrap_or(Value::Null);
        if body.get("messages").and_then(Value::as_array).is_none() {
            continue;
        }
        let session = envelope
            .get("session_key")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let seq = envelope.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let request_id = envelope
            .get("request_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{session}-{seq:010}"));
        sessions.entry(session).or_default().push(Turn {
            seq,
            request_id,
            body,
        });
    }
    for turns in sessions.values_mut() {
        turns.sort_by_key(|t| t.seq);
    }
    sessions
}

/// Replay every session through a fresh store, mirroring the live order:
/// look up the previous turn, overlay, park unconditionally (side errands
/// excepted, as live), commit. Returns the hit decision per turn.
fn replay_rate(sessions: &BTreeMap<String, Vec<Turn>>) -> Rate {
    let store = SessionReplayStore::new(64);
    let mut rate = Rate::default();
    // `session` lives on the map key; keep the per-turn struct small.
    for (session, turns) in sessions {
        for turn in turns {
            let originals: Vec<Value> = turn.body["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            rate.turns += 1;

            let prev = store.previous_turn_for(session, &originals, None);
            let (prev_orig, prev_fwd, chain_id) = match prev {
                Ok((o, f, chain)) => (Some(o), Some(f), chain),
                Err(miss) => {
                    *rate
                        .prefix_miss
                        .entry(miss.as_str().to_string())
                        .or_default() += 1;
                    park(&store, session, &turn.request_id, &originals, &originals);
                    continue;
                }
            };
            let floor = store.confirmed_frozen_count(session);
            let (overlaid, skip) = overlay_cached_prefix_reported(
                originals.clone(),
                &originals,
                prev_orig.as_deref(),
                prev_fwd.as_deref(),
                chain_id != 0,
                Some(floor),
            );
            match skip {
                None => rate.applied += 1,
                Some(reason) => {
                    *rate
                        .replay_skip
                        .entry(reason.as_str().to_string())
                        .or_default() += 1;
                }
            }
            park(&store, session, &turn.request_id, &originals, &overlaid);
        }
    }
    rate
}

/// Park this turn as the session's previous turn and commit it, as the live
/// response side does. Placement is bypassed (it never changes the hit
/// decision); the parked pair is what the next turn compares against.
fn park(
    store: &SessionReplayStore,
    session: &str,
    request_id: &str,
    originals: &[Value],
    forwarded: &[Value],
) {
    if is_side_errand(originals) {
        return;
    }
    store.begin_request(
        request_id,
        session,
        originals.to_vec(),
        forwarded.to_vec(),
        // Fixtures carry no system block; `None` at the lookup already
        // skipped the lineage precondition, so the parked hash is inert.
        String::new(),
    );
    store.complete(request_id, 0, 0);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut dir = String::new();
    let mut expected: Option<String> = None;
    while let Some(arg) = args.next() {
        if arg == "--expected" {
            expected = Some(args.next().unwrap_or_else(|| {
                eprintln!("expected a path after --expected");
                std::process::exit(2);
            }));
            continue;
        }
        if dir.is_empty() {
            dir = arg;
        } else {
            eprintln!("unexpected argument {arg}");
            std::process::exit(2);
        }
    }
    if dir.is_empty() {
        eprintln!("usage: prefix_replay_rate <corpus_dir> [--expected expected.json]");
        std::process::exit(2);
    }
    let sessions = load_corpus(Path::new(&dir));
    if sessions.is_empty() {
        eprintln!("no turns found in {dir}");
        std::process::exit(1);
    }
    let rate = replay_rate(&sessions);
    println!(
        "turns={} applied={} hit_rate={:.4}",
        rate.turns,
        rate.applied,
        rate.hit_rate()
    );
    let mut reasons: Vec<(&String, &usize)> = rate
        .prefix_miss
        .iter()
        .chain(rate.replay_skip.iter())
        .collect();
    reasons.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (reason, count) in reasons {
        println!("  miss {reason}: {count}");
    }

    if let Some(expected_path) = expected {
        let golden: Value =
            serde_json::from_slice(&std::fs::read(&expected_path).unwrap_or_else(|e| {
                eprintln!("read {expected_path}: {e}");
                std::process::exit(1);
            }))
            .unwrap_or_else(|e| {
                eprintln!("parse {expected_path}: {e}");
                std::process::exit(1);
            });
        let mut ok = true;
        // Totals only: exact per-reason strings are printed above for humans
        // but not pinned, so a renamed reason can't fail the gate while the
        // decision it names still holds.
        for (key, got) in [
            ("turns", rate.turns as u64),
            ("applied", rate.applied as u64),
            (
                "prefix_miss_total",
                rate.prefix_miss.values().map(|c| *c as u64).sum(),
            ),
            (
                "replay_skip_total",
                rate.replay_skip.values().map(|c| *c as u64).sum(),
            ),
        ] {
            let want = golden.get(key).and_then(Value::as_u64).unwrap_or(u64::MAX);
            if want != got {
                eprintln!("MISMATCH {key}: golden {want}, replay {got}");
                ok = false;
            }
        }
        if !ok {
            std::process::exit(1);
        }
        println!("matches golden {expected_path}");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use std::path::PathBuf;
    /// Recache gate: the synthetic corpus pins the hit decision. Reasoned in
    /// the fixture README if there is one, else per file: steady growth
    /// applies after the cold turn; an edited history diverges; a lone turn
    /// misses cold. Any derivation or canonicalization change moves this rate
    /// and must arrive as a versioned migration, not a silent drift.
    #[test]
    fn synthetic_corpus_hit_rate_is_pinned() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay_corpus");
        let sessions = super::load_corpus(&dir);
        assert!(!sessions.is_empty(), "missing {}", dir.display());
        let rate = super::replay_rate(&sessions);
        let golden: Value = serde_json::from_slice(
            &std::fs::read(dir.join("expected.json")).expect("golden present"),
        )
        .expect("golden parses");
        assert_eq!(
            rate.turns as u64,
            golden["turns"].as_u64().expect("golden turns"),
            "corpus changed shape: {:?}",
            rate
        );
        assert_eq!(
            rate.applied as u64,
            golden["applied"].as_u64().expect("golden applied"),
            "hit decision moved: {:?}",
            rate
        );
        for (key, got) in [
            ("turns", rate.turns as u64),
            ("applied", rate.applied as u64),
            (
                "prefix_miss_total",
                rate.prefix_miss.values().map(|c| *c as u64).sum(),
            ),
            (
                "replay_skip_total",
                rate.replay_skip.values().map(|c| *c as u64).sum(),
            ),
        ] {
            let want = golden.get(key).and_then(Value::as_u64).unwrap_or(u64::MAX);
            assert_eq!(got, want, "{key} moved: {rate:?}");
        }
    }
}
