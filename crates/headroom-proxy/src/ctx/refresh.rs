//! Jev context refresh: observe mode scores and logs, apply mode rewrites.
//!
//! Spark sessions grow to the model's window and then fail with a generic 400.
//! The idea (`docs/notes/ideas/jev-context-refresh.md`) is to ask Jev which
//! earlier exchanges the current task still needs and stub the rest. Observe
//! mode only scores and logs, so the choices can be read before anything is
//! applied; apply mode builds the shorter base in the background and later
//! turns forward that base plus the new tail.
//!
//! The unit is an exchange (one typed user message and everything up to the
//! next) before the current task, and a tool round from the task's exchange
//! on: one assistant turn and the user messages that answer it, the first
//! round carrying the typed message. On the 1M-token session of 2026-10-01,
//! tool results were 20% of the bytes and user-role text 43%, so scoring tool
//! results alone would not have shrunk it. Whole exchanges alone left a long
//! autonomous run, one exchange that is never stubbed, nothing to cut (0-1% in
//! three of five sessions, 2026-10-12 observe data). Rounds everywhere cut the
//! largest multi-exchange history's estimate from 18% to 5%, since
//! [`MAX_SCORED`] then covers small rounds, not large exchanges. A round
//! boundary only falls on an assistant message, so a `tool_result` stays in
//! the unit of the `tool_use` it answers.
//!
//! The free Jev endpoint rate-limits (429 after about 140 calls in a few
//! minutes), so a run scores at most [`MAX_SCORED`] units, three at a time,
//! and a session is observed at most once per [`COOLDOWN`].

use crate::proxy::AppState;
use futures::StreamExt;
use headroom_core::ccr::{CcrStore, compute_key};
use headroom_core::transforms::live_zone::CTX_OFFLOAD_MARKER_PREFIX;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const JEV_MODEL: &str = "jev-1.13-free";
const QUESTION: &str =
    "Does the current task need the details of this earlier exchange to be done correctly?";
/// The 1M-token session of 2026-10-01 was 4.5 MB on the wire.
const BYTES_PER_TOKEN: usize = 4;
/// Units never scored: the opening one, and the newest few.
const PROTECT_HEAD: usize = 1;
const PROTECT_TAIL: usize = 5;
const MAX_SCORED: usize = 100;
/// A typed request shorter than this ("continue", "ok") is not a task.
const TASK_MIN_CHARS: usize = 20;
const CONCURRENCY: usize = 3;
const COOLDOWN: Duration = Duration::from_secs(30 * 60);
/// A base unreplayed this long is dropped: the provider's cached prefix is
/// cold by then, and compaction moves a session to a new key, so the old
/// base would otherwise stay in memory for the life of the process.
const BASE_IDLE: Duration = Duration::from_secs(2 * 60 * 60);
/// Bases kept at most, least recently replayed out first. One is about the
/// session's size on the wire, a few MB at the 1M ceiling.
const MAX_BASES: usize = 16;
const RATE_LIMIT_RETRIES: u32 = 5;
const JEV_TIMEOUT: Duration = Duration::from_secs(30);
const TASK_CHARS: usize = 1500;
const EXCHANGE_CHARS: usize = 5000;
const PART_CHARS: usize = 400;
/// Scores under each of these would be stubbed; the log reports all three.
const THRESHOLDS: [f64; 3] = [0.3, 0.4, 0.5];
/// Apply mode stubs the lowest-scoring `1 / STUB_QUARTILE_DIVISOR` of scored
/// exchanges, never a fixed cutoff: on the 1M-token session the large
/// exchanges scored 0.48-0.72 while trivia scored under 0.3, so a threshold
/// keeps what is big, and the mid-range moves with the task wording (same
/// ranking, shifted scores). Percentiles transfer; thresholds do not.
const STUB_QUARTILE_DIVISOR: usize = 4;
/// User text that is not a request from the person typing.
const NOT_TYPED: [&str; 5] = [
    "<task-notification>",
    "Stop hook feedback",
    "[Request interrupted",
    "<local-command",
    "This session is being continued",
];

/// An exchange or a tool round: `messages[first..end]`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Unit {
    pub first: usize,
    pub end: usize,
    /// The typed request of the exchange this unit belongs to.
    pub typed: String,
    /// True for a whole exchange or an exchange's first round, which hold
    /// the typed message.
    pub opens: bool,
    /// What the stub and the log show: the request for an opening round, the
    /// tools called and the assistant's text for a later one.
    pub head: String,
    pub bytes: usize,
    pub text: String,
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Text a person typed in this message: none for a tool result, a reminder or
/// a harness notice. A message carrying any `tool_result` is none too, even
/// with text beside it: it answers the previous exchange's `tool_use`, so
/// starting an exchange there would split the pair, and a stub would drop
/// the result.
fn typed_text(msg: &Value) -> Option<String> {
    if msg.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let raw = match msg.get("content")? {
        Value::String(s) => s.clone(),
        Value::Array(blocks)
            if blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")) =>
        {
            return None;
        }
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let (typed, _) = crate::cache_stabilization::ephemeral_spans::split_ephemeral_spans(&raw);
    let typed = typed.trim();
    if typed.is_empty() || NOT_TYPED.iter().any(|p| typed.starts_with(p)) {
        return None;
    }
    Some(typed.to_string())
}

/// The task a compaction summary carries: its "Primary Request and Intent"
/// section, or the whole summary when that heading is missing. After the
/// client compacts, an agent often runs on with no typed message at all: in
/// 62 of 102 Spark histories over 100k tokens (2026-09-20..10-12), the
/// summary was the only request there was.
fn summary_task(msg: &Value) -> Option<String> {
    const SUMMARY: &str = "This session is being continued";
    const SECTION: &str = "Primary Request and Intent:";
    if msg.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let raw = match msg.get("content")? {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let (text, _) = crate::cache_stabilization::ephemeral_spans::split_ephemeral_spans(&raw);
    let text = text.trim();
    if !text.starts_with(SUMMARY) {
        return None;
    }
    let task = match text.find(SECTION) {
        Some(at) => {
            let section = &text[at + SECTION.len()..];
            section.find("\n2.").map_or(section, |end| &section[..end])
        }
        None => text,
    };
    Some(task.trim().to_string()).filter(|t| !t.is_empty())
}

/// What Jev reads for one message: text, tool calls and results, each cut short.
fn render_message(msg: &Value, out: &mut String) {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("?");
    let mut part = |label: &str, text: &str| {
        out.push_str(&format!(
            "{label}: {}\n",
            truncate_chars(text.trim(), PART_CHARS)
        ));
    };
    match msg.get("content") {
        Some(Value::String(s)) => part(role, s),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => part(role, b.get("text").and_then(Value::as_str).unwrap_or("")),
                    Some("tool_use") => part(
                        &format!(
                            "tool {}",
                            b.get("name").and_then(Value::as_str).unwrap_or("?")
                        ),
                        &b.get("input").map(Value::to_string).unwrap_or_default(),
                    ),
                    Some("tool_result") => {
                        let body = match b.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            Some(Value::Array(a)) => a
                                .iter()
                                .filter_map(|x| x.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join(" "),
                            _ => String::new(),
                        };
                        part("result", &body);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Head and tail of an over-long exchange, so its opening request and its
/// outcome both reach Jev.
fn cap_exchange(text: &str) -> String {
    if text.chars().count() <= EXCHANGE_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(EXCHANGE_CHARS * 7 / 10).collect();
    let tail_len = EXCHANGE_CHARS * 3 / 10;
    let skip = text.chars().count() - tail_len;
    let tail: String = text.chars().skip(skip).collect();
    format!("{head}\n…\n{tail}")
}

fn role(msg: &Value) -> Option<&str> {
    msg.get("role").and_then(Value::as_str)
}

/// Head of a later round: the tools it called and the assistant's first text.
fn round_head(slice: &[Value]) -> String {
    let mut tools: Vec<&str> = Vec::new();
    let mut said = "";
    for m in slice.iter().filter(|m| role(m) == Some("assistant")) {
        match m.get("content") {
            Some(Value::String(s)) if said.is_empty() => said = s,
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("tool_use") => {
                            let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
                            if !tools.contains(&name) {
                                tools.push(name);
                            }
                        }
                        Some("text") if said.is_empty() => {
                            said = b.get("text").and_then(Value::as_str).unwrap_or("");
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let said = said.trim().replace('\n', " ");
    if tools.is_empty() {
        said
    } else {
        format!("tools {}: {said}", tools.join(", "))
    }
}

/// Split a transcript into exchanges, and those from the current task's on
/// into rounds. A new round starts at an assistant message that follows a
/// user message no one typed (a tool result or a harness notice). A
/// compaction summary opens an exchange as a typed message would, its task
/// taken from [`summary_task`]. Messages before the first request belong to
/// the first unit.
pub(crate) fn units(messages: &[Value]) -> Vec<Unit> {
    let mut starts: Vec<(usize, String)> = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if let Some(t) = typed_text(m).or_else(|| summary_task(m)) {
            starts.push((i, t));
        }
    }
    let split_from = starts
        .iter()
        .rposition(|(_, t)| t.chars().count() >= TASK_MIN_CHARS)
        .unwrap_or(0);
    let mut out = Vec::new();
    for (k, (typed_at, typed)) in starts.iter().enumerate() {
        let first = if k == 0 { 0 } else { *typed_at };
        let end = starts.get(k + 1).map_or(messages.len(), |(next, _)| *next);
        let mut bounds = vec![first];
        let rounds_from = if k < split_from { end } else { typed_at + 1 };
        bounds.extend((rounds_from..end).filter(|&i| {
            i - 1 != *typed_at
                && role(&messages[i]) == Some("assistant")
                && role(&messages[i - 1]) == Some("user")
        }));
        for (r, &from) in bounds.iter().enumerate() {
            let to = bounds.get(r + 1).copied().unwrap_or(end);
            let slice = &messages[from..to];
            let opens = r == 0;
            let mut text = String::new();
            if !opens {
                text.push_str(&format!("request: {}\n", truncate_chars(typed, PART_CHARS)));
            }
            for m in slice {
                render_message(m, &mut text);
            }
            out.push(Unit {
                first: from,
                end: to,
                typed: typed.clone(),
                opens,
                head: if opens {
                    typed.clone()
                } else {
                    round_head(slice)
                },
                bytes: slice.iter().map(|m| m.to_string().len()).sum(),
                text: cap_exchange(&text),
            });
        }
    }
    out
}

/// The unit that opens the last request that says something: not a stray
/// "continue" or "ok".
fn current_task(units: &[Unit]) -> Option<usize> {
    units
        .iter()
        .rposition(|u| u.opens && u.typed.chars().count() >= TASK_MIN_CHARS)
}

/// What gets scored: everything but the opening, the newest few and the
/// round holding the task's request; the largest first, since they hold the
/// savings.
fn candidates(ex: &[Unit], task: usize) -> Vec<usize> {
    let tail_start = ex.len().saturating_sub(PROTECT_TAIL);
    let mut idx: Vec<usize> = (PROTECT_HEAD..tail_start).filter(|i| *i != task).collect();
    idx.sort_by_key(|i| std::cmp::Reverse(ex[*i].bytes));
    idx.truncate(MAX_SCORED);
    idx.sort_unstable();
    idx
}

#[derive(Debug, Clone)]
pub(crate) struct Scored {
    pub index: usize,
    pub messages: usize,
    pub tokens: usize,
    pub score: f64,
    pub head: String,
    /// A later round, not the one that opens its exchange.
    pub round: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Observation {
    pub exchanges: usize,
    pub units: usize,
    pub scored: Vec<Scored>,
    pub errors: usize,
}

/// One Jev call, sent through the Zen egress lanes like a Spark turn.
///
/// The free limit follows the exit IP (`learnings/zen-free-limit-is-per-exit-daily.md`):
/// on 2026-10-01 the machine's own address answered 429 on every call while all
/// nine lanes answered 200. A 429 moves the call to another lane; with no other
/// lane (or no pool) it pauses and tries again. Any other failure is an error
/// for this exchange only. The lane marks that steer Spark turns stay untouched:
/// whether Jev and Spark share a counter is not known.
async fn ask_jev(
    state: &AppState,
    url: &str,
    task: &str,
    exchange: &str,
    lane_key: &str,
) -> Result<f64, String> {
    let body = json!({
        "model": JEV_MODEL,
        "state": format!("Current task:\n{task}\n\nEarlier exchange:\n{exchange}"),
        "questions": {"q": {"type": "noul", "instructions": QUESTION}},
    });
    let lane_busy = |egress: String| format!("lane {egress} is rotating");
    let mut tried: Vec<usize> = Vec::new();
    let mut pauses = 0u32;
    let mut selected = state
        .zen_client_for_lane(Some(lane_key))
        .map_err(lane_busy)?;
    loop {
        let (client, slot, _egress, guard) = selected;
        let sent = client
            .post(url)
            .header(
                reqwest::header::USER_AGENT,
                concat!("headroom-proxy/", env!("CARGO_PKG_VERSION")),
            )
            .timeout(JEV_TIMEOUT)
            .json(&body)
            .send()
            .await;
        let answered = sent
            .as_ref()
            .is_ok_and(|r| r.status() != reqwest::StatusCode::TOO_MANY_REQUESTS);
        if !answered {
            let why = sent.as_ref().err().map(|e| format!("send: {e}"));
            tried.push(slot);
            drop(guard);
            if let Some(next) = state.zen_failover(slot, &tried) {
                selected = next;
                continue;
            }
            if let Some(why) = why {
                return Err(why);
            }
            if pauses == RATE_LIMIT_RETRIES {
                return Err("429".to_string());
            }
            tokio::time::sleep(retry_pause(pauses)).await;
            pauses += 1;
            tried.clear();
            selected = state
                .zen_client_for_lane(Some(lane_key))
                .map_err(lane_busy)?;
            continue;
        }
        let resp = sent.map_err(|e| format!("send: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        let v: Value = resp.json().await.map_err(|e| format!("body: {e}"))?;
        return v
            .pointer("/answers/q/noul")
            .or_else(|| v.pointer("/q/noul"))
            .and_then(Value::as_f64)
            .ok_or_else(|| "no noul answer".to_string());
    }
}

fn retry_pause(attempt: u32) -> Duration {
    #[cfg(test)]
    {
        Duration::from_millis(1 + u64::from(attempt))
    }
    #[cfg(not(test))]
    {
        Duration::from_secs(3 * u64::from(attempt + 1))
    }
}

/// Score the candidates against the current task.
pub(crate) async fn observe_units(state: &AppState, url: &str, ex: &[Unit]) -> Observation {
    let mut obs = Observation {
        exchanges: ex.iter().filter(|u| u.opens).count(),
        units: ex.len(),
        ..Default::default()
    };
    let Some(task_idx) = current_task(ex) else {
        return obs;
    };
    let task = truncate_chars(&ex[task_idx].typed, TASK_CHARS);
    let results: Vec<(usize, Result<f64, String>)> =
        futures::stream::iter(candidates(ex, task_idx))
            .map(|i| {
                let task = &task;
                // Spread the calls over lanes: one key per concurrent slot.
                let lane_key = format!("jev-refresh-{}", i % CONCURRENCY);
                async move { (i, ask_jev(state, url, task, &ex[i].text, &lane_key).await) }
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;
    for (i, r) in results {
        match r {
            Ok(score) => obs.scored.push(Scored {
                index: i,
                messages: ex[i].end - ex[i].first,
                tokens: ex[i].bytes / BYTES_PER_TOKEN,
                score,
                head: truncate_chars(&ex[i].head.replace('\n', " "), 100),
                round: !ex[i].opens,
            }),
            Err(_) => obs.errors += 1,
        }
    }
    obs.scored.sort_by_key(|s| s.index);
    obs
}

/// In-flight and recently observed sessions.
fn seen() -> &'static Mutex<HashMap<String, Instant>> {
    static SEEN: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    SEEN.get_or_init(Default::default)
}

/// True when this session may start a run now, and marks it started.
fn claim(session_key: &str) -> bool {
    let Ok(mut map) = seen().lock() else {
        return false;
    };
    let now = Instant::now();
    map.retain(|_, t| now.duration_since(*t) < COOLDOWN);
    if map.contains_key(session_key) {
        return false;
    }
    map.insert(session_key.to_string(), now);
    true
}

fn log_observation(session_key: &str, request_id: &str, total_tokens: usize, obs: &Observation) {
    let session = crate::cache_stabilization::drift_detector::session_key_log_prefix(session_key);
    for s in &obs.scored {
        tracing::info!(
            event = "ctx_refresh_exchange",
            session_key_hash = %session,
            request_id = %request_id,
            index = s.index,
            messages = s.messages,
            tokens = s.tokens,
            score = s.score,
            head = %s.head,
            round = s.round,
            "Jev scored an earlier round against the current task"
        );
    }
    let under = |t: f64| {
        obs.scored
            .iter()
            .filter(|s| s.score < t)
            .fold((0usize, 0usize), |(n, tok), s| (n + 1, tok + s.tokens))
    };
    let (n3, t3) = under(THRESHOLDS[0]);
    let (n4, t4) = under(THRESHOLDS[1]);
    let (n5, t5) = under(THRESHOLDS[2]);
    tracing::info!(
        event = "ctx_refresh_observed",
        session_key_hash = %session,
        request_id = %request_id,
        total_tokens,
        exchanges = obs.exchanges,
        units = obs.units,
        scored = obs.scored.len(),
        errors = obs.errors,
        stub_lt_0_3_units = n3,
        stub_lt_0_3_tokens = t3,
        stub_lt_0_4_units = n4,
        stub_lt_0_4_tokens = t4,
        stub_lt_0_5_units = n5,
        stub_lt_0_5_tokens = t5,
        "Jev refresh observed; the request was not changed"
    );
}

/// Entry point from the routed request path. Never changes `parsed` and never
/// waits: past the trigger it clones the messages and scores them in the
/// background.
pub(crate) fn observe(state: &AppState, parsed: &Value, session_key: &str, request_id: &str) {
    let trigger = state.config.ctx_refresh_observe_tokens;
    if trigger == 0 {
        return;
    }
    // Spark only: the client's model name carries it (`claude-muse-spark-1.3`).
    let spark = parsed
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|m| m.to_lowercase().contains("spark"));
    let Some(messages) = parsed.get("messages").filter(|_| spark) else {
        return;
    };
    let total_tokens = messages.to_string().len() / BYTES_PER_TOKEN;
    if total_tokens < trigger || !claim(session_key) {
        return;
    }
    let mut body = json!({ "messages": messages.clone() });
    if state.config.redact_sensitive {
        // The history goes to opencode.ai, as the Spark turn itself does; send
        // it as that turn does, redacted.
        crate::redact::redact_body(&state.redact_store, session_key, &mut body);
    }
    let url = state.config.ctx_refresh_jev_url.clone();
    let state = state.clone();
    let (session_key, request_id) = (session_key.to_string(), request_id.to_string());
    tokio::spawn(async move {
        let msgs = body["messages"].as_array().cloned().unwrap_or_default();
        let ex = units(&msgs);
        let obs = observe_units(&state, &url, &ex).await;
        log_observation(&session_key, &request_id, total_tokens, &obs);
    });
}

/// A shorter history built once and replayed verbatim, `ctx/inject.rs` style:
/// rewriting the prefix identically every turn keeps the provider's cached
/// prefix stable. The live tail (`base_len..`) always goes through as-is.
#[derive(Debug, Clone)]
struct RefreshBase {
    /// Rewritten prefix messages.
    base: Vec<Value>,
    /// How many of the turn's original messages `base` covers.
    base_len: usize,
    /// `compute_key` of the first message when built. A shorter history means
    /// the client cleared; a same-length history with a different first
    /// message means it compacted, and this base no longer covers it.
    first_hash: String,
    /// `compute_key` of the last message the base covers
    /// (`base_len - 1`). A rewind can keep the length and the first message
    /// yet change this one; without the check the base would overwrite it.
    last_hash: String,
    /// Size of the history the base was built from, in estimated tokens.
    built_tokens: usize,
    /// Last build or replay; [`prune_bases`] drops the idle ones.
    last_used: Instant,
}

/// Drop bases idle past [`BASE_IDLE`], then the least recently used until at
/// most [`MAX_BASES`] remain.
fn prune_bases(map: &mut HashMap<String, RefreshBase>, now: Instant) {
    map.retain(|_, b| now.duration_since(b.last_used) < BASE_IDLE);
    while map.len() > MAX_BASES {
        let Some(oldest) = map
            .iter()
            .min_by_key(|(_, b)| b.last_used)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        map.remove(&oldest);
    }
}

/// True when a session's history has grown enough since its base was built
/// to be worth another. A rebuild re-scores, Jev's scores move a little from
/// run to run, and any change to the replayed prefix costs a cache miss; so
/// wait until the history has grown by the share one build stubs.
fn grew_enough(base: Option<&RefreshBase>, total_tokens: usize) -> bool {
    base.is_none_or(|b| total_tokens >= b.built_tokens + b.built_tokens / STUB_QUARTILE_DIVISOR)
}

fn message_hash(msg: Option<&Value>) -> String {
    compute_key(msg.map(Value::to_string).unwrap_or_default().as_bytes())
}

/// Persisted bases by routed session key. Process-local: a restart rebuilds
/// the base on the next trigger turn (the stubbed originals stay in the CCR
/// store under their hashes either way).
fn bases() -> &'static Mutex<HashMap<String, RefreshBase>> {
    static BASES: OnceLock<Mutex<HashMap<String, RefreshBase>>> = OnceLock::new();
    BASES.get_or_init(Default::default)
}

/// Sessions with a base build running or recently finished.
fn build_seen() -> &'static Mutex<HashMap<String, Instant>> {
    static BUILD_SEEN: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    BUILD_SEEN.get_or_init(Default::default)
}

/// True when this session may start a base build now, and marks it started.
/// Separate from [`claim`]: observe and apply run on their own triggers. A
/// rebuild needs both the [`COOLDOWN`] and [`grew_enough`].
fn claim_build(session_key: &str) -> bool {
    let Ok(mut map) = build_seen().lock() else {
        return false;
    };
    let now = Instant::now();
    map.retain(|_, t| now.duration_since(*t) < COOLDOWN);
    if map.contains_key(session_key) {
        return false;
    }
    map.insert(session_key.to_string(), now);
    true
}

/// The lowest-scoring quarter of `scored`, as exchange indices, ascending.
/// Pure so the rule is testable without Jev.
fn select_stubs(scored: &[Scored]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..scored.len()).collect();
    order.sort_by(|&a, &b| {
        scored[a]
            .score
            .total_cmp(&scored[b].score)
            .then_with(|| scored[a].index.cmp(&scored[b].index))
    });
    let mut stubs: Vec<usize> = order
        .into_iter()
        .take(scored.len() / STUB_QUARTILE_DIVISOR)
        .map(|i| scored[i].index)
        .collect();
    stubs.sort_unstable();
    stubs
}

/// The replacement for one stubbed round: a pointer, not prose. Jev cannot
/// summarise, so the stub says what was dropped and where the original lives;
/// `headroom_retrieve` fetches `<<ctx:HASH>>` from the CCR store.
fn stub_text(
    opens: bool,
    score: f64,
    head: &str,
    messages: usize,
    tokens: usize,
    hash: &str,
) -> String {
    let (what, shown, label) = if opens {
        ("exchange", "its opening request", "Opening request")
    } else {
        ("tool round", "the tools it called", "Tool round")
    };
    format!(
        "[Context refresh: an earlier {what} ({messages} messages, about {tokens} tokens, relevance {score:.2}) was stubbed; only {shown} is shown below. The full {what} is kept and can be fetched back.]\n{label}: {head}\n{CTX_OFFLOAD_MARKER_PREFIX}{hash}>> (stubbed {what}; use headroom_retrieve to fetch the original)"
    )
}

/// Rewrite `messages[..base_len]` with each stubbed round replaced by one
/// user message carrying the stub. A round holds every `tool_result` that
/// answers its `tool_use`, so no pair is split. Returns the rewritten prefix
/// plus stubbed counts. A round whose original the store refuses stays
/// verbatim: a stub without an original is dropped history, not refresh.
fn rewrite_prefix(
    messages: &[Value],
    ex: &[Unit],
    scored: &[Scored],
    base_len: usize,
    ccr: &dyn CcrStore,
) -> (Vec<Value>, usize, usize) {
    let stubs = select_stubs(scored);
    let score_of = |index: usize| {
        scored
            .iter()
            .find(|s| s.index == index)
            .map_or(0.0, |s| s.score)
    };
    let mut out = Vec::with_capacity(base_len);
    let mut stubbed = 0;
    let mut stubbed_tokens = 0;
    for (index, e) in ex.iter().enumerate() {
        if e.first >= base_len {
            break;
        }
        let end = e.end.min(base_len).min(messages.len());
        if end <= e.first {
            continue;
        }
        let slice = &messages[e.first..end];
        if !stubs.contains(&index) {
            out.extend_from_slice(slice);
            continue;
        }
        let original = Value::Array(slice.to_vec()).to_string();
        let hash = compute_key(original.as_bytes());
        if !ccr.put(&hash, &original) {
            out.extend_from_slice(slice);
            continue;
        }
        let tokens = original.len() / BYTES_PER_TOKEN;
        out.push(json!({"role": "user", "content": stub_text(
            e.opens,
            score_of(index),
            &truncate_chars(&e.head.replace('\n', " "), 100),
            slice.len(),
            tokens,
            &hash,
        )}));
        stubbed += 1;
        stubbed_tokens += tokens;
    }
    (out, stubbed, stubbed_tokens)
}

/// Score one snapshot and persist the rewritten base. Runs off the request
/// path; the next turn replays what it stores. The newest [`PROTECT_TAIL`]
/// rounds stay out of the base: they are the live tail, always verbatim.
#[allow(clippy::too_many_arguments)]
async fn build_base(
    state: &AppState,
    url: &str,
    snapshot: &Value,
    session_key: &str,
    request_id: &str,
    total_tokens: usize,
    redact: bool,
    ccr: Arc<dyn CcrStore>,
) {
    let live = snapshot.as_array().cloned().unwrap_or_default();
    let mut body = json!({ "messages": snapshot.clone() });
    if redact {
        crate::redact::redact_body(&state.redact_store, session_key, &mut body);
    }
    let judged = body["messages"].as_array().cloned().unwrap_or_default();
    if judged.len() != live.len() {
        // Redaction must not reshape the transcript; if it ever does, the
        // judged positions would stub the wrong messages. Abort, keep history.
        return;
    }
    let ex = units(&judged);
    if ex.len() <= PROTECT_HEAD + PROTECT_TAIL {
        return;
    }
    let obs = observe_units(state, url, &ex).await;
    if obs.scored.is_empty() || select_stubs(&obs.scored).is_empty() {
        return;
    }
    let base_len = ex[ex.len() - PROTECT_TAIL].first;
    let (base, stubbed, stubbed_tokens) =
        rewrite_prefix(&live, &ex, &obs.scored, base_len, ccr.as_ref());
    if stubbed == 0 {
        return;
    }
    let first_hash = message_hash(live.first());
    let last_hash = message_hash(base_len.checked_sub(1).and_then(|i| live.get(i)));
    let base_messages = base.len();
    if let Ok(mut map) = bases().lock() {
        let now = Instant::now();
        map.insert(
            session_key.to_string(),
            RefreshBase {
                base,
                base_len,
                first_hash,
                last_hash,
                built_tokens: total_tokens,
                last_used: now,
            },
        );
        prune_bases(&mut map, now);
    }
    let session = crate::cache_stabilization::drift_detector::session_key_log_prefix(session_key);
    tracing::info!(
        event = "ctx_refresh_base_built",
        session_key_hash = %session,
        request_id = %request_id,
        total_tokens = total_tokens,
        exchanges = obs.exchanges,
        units = obs.units,
        scored = obs.scored.len(),
        stubbed_units = stubbed,
        stubbed_tokens = stubbed_tokens,
        base_len = base_len,
        base_messages = base_messages,
        tail_messages = live.len() - base_len,
        errors = obs.errors,
        "Jev refresh base built and persisted; replayed from the next turn"
    );
}

/// Apply-mode entry point from the routed request path. Replays the stored
/// base plus the live tail when one covers this history, and past the trigger
/// starts a background build of a new base. Returns true when `parsed` was
/// rewritten. Replay is synchronous; scoring never blocks the turn: a fresh
/// base lands on the first turn after the background run finishes.
pub(crate) fn apply(
    state: &AppState,
    parsed: &mut Value,
    session_key: &str,
    request_id: &str,
) -> bool {
    let trigger = state.config.ctx_refresh_apply_tokens;
    if trigger == 0 {
        return false;
    }
    // Spark only, like the observer.
    let spark = parsed
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|m| m.to_lowercase().contains("spark"));
    let Some(messages) = parsed.get("messages").filter(|_| spark) else {
        return false;
    };
    let total_tokens = messages.to_string().len() / BYTES_PER_TOKEN;
    let base = bases().lock().ok().and_then(|mut m| {
        let b = m.get_mut(session_key)?;
        b.last_used = Instant::now();
        Some(b.clone())
    });
    // A build needs the CCR store: stubs without retrievable originals are
    // dropped history, not refresh.
    if total_tokens >= trigger
        && grew_enough(base.as_ref(), total_tokens)
        && let Some(ccr) = state.ctx_offload.as_ref().map(|r| r.store.ccr())
        && claim_build(session_key)
    {
        let snapshot = messages.clone();
        let url = state.config.ctx_refresh_jev_url.clone();
        let redact = state.config.redact_sensitive;
        let state = state.clone();
        let (session_key, request_id) = (session_key.to_string(), request_id.to_string());
        tokio::spawn(async move {
            build_base(
                &state,
                &url,
                &snapshot,
                &session_key,
                &request_id,
                total_tokens,
                redact,
                ccr,
            )
            .await;
        });
    }
    // Replay AFTER the build clones its snapshot: the build scores the full
    // history, this turn forwards the stored base plus the live tail.
    let Some(base) = base else {
        return false;
    };
    let stale = match parsed.get("messages").and_then(Value::as_array) {
        Some(msgs) if msgs.len() >= base.base_len => {
            let last = base.base_len.checked_sub(1).and_then(|i| msgs.get(i));
            message_hash(msgs.first()) != base.first_hash || message_hash(last) != base.last_hash
        }
        _ => true,
    };
    if stale {
        // The client cleared, compacted or rewound under us; drop the base
        // and let the next trigger turn rebuild it.
        if let Ok(mut m) = bases().lock() {
            m.remove(session_key);
        }
        return false;
    }
    let mut next = base.base.clone();
    let mut tail_messages = 0;
    if let Some(msgs) = parsed.get("messages").and_then(Value::as_array) {
        tail_messages = msgs.len() - base.base_len;
        next.extend(msgs[base.base_len..].iter().cloned());
    }
    parsed["messages"] = Value::Array(next);
    let tokens_after = parsed["messages"].to_string().len() / BYTES_PER_TOKEN;
    let session = crate::cache_stabilization::drift_detector::session_key_log_prefix(session_key);
    tracing::info!(
        event = "ctx_refresh_applied",
        session_key_hash = %session,
        request_id = %request_id,
        base_len = base.base_len,
        base_messages = base.base.len(),
        tail_messages = tail_messages,
        tokens_before = total_tokens,
        tokens_after = tokens_after,
        "Jev refresh base replayed with the live tail"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": text})
    }
    fn assistant_tool(id: &str) -> Value {
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "reading"},
            {"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": "/a.rs"}}]})
    }
    fn tool_result(id: &str, body: &str) -> Value {
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": body}]})
    }

    /// 10 typed requests, each followed by a tool round trip.
    fn transcript() -> Vec<Value> {
        let mut m = Vec::new();
        for i in 0..10 {
            m.push(user(&format!(
                "please do the thing number {i} in the parser"
            )));
            m.push(assistant_tool(&format!("t{i}")));
            m.push(tool_result(&format!("t{i}"), "ok"));
        }
        m
    }

    #[test]
    fn a_tool_result_does_not_start_an_exchange() {
        let ex = units(&transcript());
        assert_eq!(ex.len(), 10);
        assert!(ex.iter().all(|e| e.end - e.first == 3));
        assert_eq!((ex[0].first, ex[9].end), (0, 30));
    }

    #[test]
    fn harness_text_and_reminders_are_not_typed_requests() {
        let mut m = vec![json!({"role": "user", "content": [
            {"type": "text", "text": "<system-reminder>claude.md digest</system-reminder>"},
            {"type": "text", "text": "first real request"}]})];
        m.push(assistant_tool("a"));
        m.push(tool_result("a", "ok"));
        m.push(user(
            "<task-notification><task-id>x</task-id></task-notification>",
        ));
        m.push(user("Stop hook feedback: doc drift"));
        m.push(user("[Request interrupted by user]"));
        m.push(user("a second real request"));
        let ex = units(&m);
        assert_eq!(ex.len(), 2);
        assert_eq!(ex[0].typed, "first real request", "reminder span is lifted");
        assert_eq!(
            ex[0].end, 6,
            "notices stay inside the exchange they interrupt"
        );
    }

    /// Text typed alongside a tool result stays in the exchange whose
    /// `tool_use` the result answers: a stub of that message would replace
    /// the result with a string and orphan the call.
    #[test]
    fn text_beside_a_tool_result_does_not_start_an_exchange() {
        let mut m = transcript();
        m[5] = json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
            {"type": "text", "text": "also please handle the edge case here"}]});
        let ex = units(&m);
        assert_eq!(ex.len(), 10);
        assert_eq!((ex[1].first, ex[1].end), (3, 6));
    }

    #[test]
    fn messages_before_the_first_typed_one_join_the_first_exchange() {
        let mut m = vec![assistant_tool("a"), tool_result("a", "ok")];
        m.push(user("then a typed request that is long enough"));
        let ex = units(&m);
        assert_eq!(ex.len(), 1);
        assert_eq!((ex[0].first, ex[0].end), (0, 3));
    }

    #[test]
    fn no_typed_message_means_no_exchanges() {
        assert!(units(&[assistant_tool("a"), tool_result("a", "ok")]).is_empty());
        assert!(units(&[]).is_empty());
    }

    #[test]
    fn the_opening_the_tail_and_the_task_are_never_scored() {
        let ex = units(&transcript());
        let task = current_task(&ex).unwrap();
        assert_eq!(task, 9);
        let c = candidates(&ex, task);
        assert_eq!(c, vec![1, 2, 3, 4], "0 is the opening; 5..=9 are the tail");
    }

    #[test]
    fn a_short_last_message_is_not_the_task() {
        let mut m = transcript();
        m.push(user("continue"));
        let ex = units(&m);
        assert_eq!(current_task(&ex), Some(9), "'continue' says nothing");
    }

    #[test]
    fn a_long_exchange_keeps_its_head_and_its_tail() {
        let long = format!("{}MIDDLE{}", "a".repeat(6000), "z".repeat(6000));
        let capped = cap_exchange(&long);
        assert!(capped.starts_with("aaa") && capped.ends_with("zzz"));
        assert!(!capped.contains("MIDDLE") && capped.chars().count() < 5100);
    }

    async fn jev(responses: Vec<ResponseTemplate>) -> MockServer {
        let server = MockServer::start().await;
        let n = std::sync::atomic::AtomicUsize::new(0);
        Mock::given(method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                let i = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                responses[i.min(responses.len() - 1)].clone()
            })
            .mount(&server)
            .await;
        server
    }

    fn answer(score: f64) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"q": {"type": "noul", "noul": score}}))
    }

    #[tokio::test]
    async fn exchanges_are_scored_against_the_current_task() {
        let server = jev(vec![answer(0.2)]).await;
        let ex = units(&transcript());
        let obs = observe_units(&state_with(1), &server.uri(), &ex).await;
        assert_eq!((obs.scored.len(), obs.errors), (4, 0));
        let sent = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
        assert_eq!(body["model"], JEV_MODEL);
        let state = body["state"].as_str().unwrap();
        assert!(state.contains("Current task:\nplease do the thing number 9"));
        assert!(state.contains("Earlier exchange:"));
        assert_eq!(body["questions"]["q"]["type"], "noul");
    }

    #[tokio::test]
    async fn a_rate_limit_is_retried_then_counted_not_fatal() {
        let limited = ResponseTemplate::new(429).set_body_string("slow down");
        let server = jev(vec![limited.clone(), limited, answer(0.7)]).await;
        let ex = units(&transcript());
        let obs = observe_units(&state_with(1), &server.uri(), &ex).await;
        assert_eq!(
            (obs.scored.len(), obs.errors),
            (4, 0),
            "two 429s then success"
        );

        let always = jev(vec![ResponseTemplate::new(429)]).await;
        let obs = observe_units(&state_with(1), &always.uri(), &ex).await;
        assert_eq!((obs.scored.len(), obs.errors), (0, 4));
    }

    /// The free limit follows the exit IP. Lane A answers 429, lane B answers:
    /// every call must end up on B, none lost, none left holding a lane.
    #[tokio::test]
    async fn a_429_moves_the_call_to_another_lane() {
        let limited = jev(vec![ResponseTemplate::new(429)]).await;
        let open = jev(vec![answer(0.2)]).await;
        // Each lane is an HTTP proxy to its own mock, so the same URL reaches
        // a different server per lane.
        let lane = |mock: &MockServer| {
            crate::ssl_context::client_builder()
                .proxy(reqwest::Proxy::all(mock.uri()).unwrap())
                .build()
                .unwrap()
        };
        let pool = std::sync::Arc::new(crate::proxy::ProviderEgressPool::new(
            vec![lane(&limited), lane(&open)],
            vec!["proxy-a".to_string(), "proxy-b".to_string()],
        ));
        let mut state = state_with(1);
        state.zen_egresses = Some(pool);
        let ex = units(&transcript());
        let obs = observe_units(&state, "http://jev.invalid/systemone", &ex).await;
        assert_eq!((obs.scored.len(), obs.errors), (4, 0));
        assert_eq!(open.received_requests().await.unwrap().len(), 4);
        assert!(
            !limited.received_requests().await.unwrap().is_empty(),
            "lane A was tried first for at least one call"
        );
        let in_flight = state.zen_egress_in_flight();
        assert_eq!(in_flight["proxy-a"].as_u64(), Some(0));
        assert_eq!(in_flight["proxy-b"].as_u64(), Some(0));
    }

    /// Every lane limited: a bounded number of pauses, then an error for that
    /// exchange, and the guards are released.
    #[tokio::test]
    async fn every_lane_limited_is_an_error_per_exchange_not_a_hang() {
        let limited = jev(vec![ResponseTemplate::new(429)]).await;
        let lane = crate::ssl_context::client_builder()
            .proxy(reqwest::Proxy::all(limited.uri()).unwrap())
            .build()
            .unwrap();
        let pool = std::sync::Arc::new(crate::proxy::ProviderEgressPool::new(
            vec![lane.clone(), lane],
            vec!["proxy-a".to_string(), "proxy-b".to_string()],
        ));
        let mut state = state_with(1);
        state.zen_egresses = Some(pool);
        let ex = units(&transcript());
        let obs = observe_units(&state, "http://jev.invalid/systemone", &ex).await;
        assert_eq!((obs.scored.len(), obs.errors), (0, 4));
    }

    #[tokio::test]
    async fn a_server_error_or_a_bad_body_costs_one_exchange_not_the_run() {
        let server = jev(vec![
            ResponseTemplate::new(500),
            ResponseTemplate::new(200).set_body_string("not json"),
            ResponseTemplate::new(200).set_body_json(json!({"q": {"type": "noul"}})),
            answer(0.9),
        ])
        .await;
        let ex = units(&transcript());
        let obs = observe_units(&state_with(1), &server.uri(), &ex).await;
        assert_eq!((obs.scored.len(), obs.errors), (1, 3));
        assert!((obs.scored[0].score - 0.9).abs() < 1e-9);
    }

    #[tokio::test]
    async fn nothing_is_asked_without_a_task() {
        let server = jev(vec![answer(0.5)]).await;
        let obs = observe_units(&state_with(1), &server.uri(), &[]).await;
        assert_eq!((obs.scored.len(), obs.errors), (0, 0));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn a_session_is_claimed_once_per_cooldown() {
        assert!(claim("refresh-test-session-a"));
        assert!(!claim("refresh-test-session-a"));
        assert!(claim("refresh-test-session-b"));
    }

    fn state_with(trigger: usize) -> AppState {
        crate::test_support::test_state(move |c| c.ctx_refresh_observe_tokens = trigger)
    }

    fn spark_body(model: &str) -> Value {
        json!({"model": model, "messages": transcript()})
    }

    /// Off by default, Spark only, and only past the trigger: the three ways a
    /// turn must not start a run. A run would claim the session, so a later
    /// `claim` succeeding proves none started.
    #[tokio::test]
    async fn off_non_spark_and_under_trigger_start_nothing() {
        observe(
            &state_with(0),
            &spark_body("claude-muse-spark-1.3"),
            "gate-off",
            "r",
        );
        observe(
            &state_with(1),
            &spark_body("claude-sonnet-5"),
            "gate-model",
            "r",
        );
        observe(
            &state_with(10_000_000),
            &spark_body("claude-muse-spark-1.3"),
            "gate-under",
            "r",
        );
        assert!(claim("gate-off") && claim("gate-model") && claim("gate-under"));
    }

    #[tokio::test]
    async fn a_spark_turn_past_the_trigger_claims_its_session_and_leaves_the_body_alone() {
        let body = spark_body("claude-muse-spark-1.3");
        let before = body.clone();
        observe(&state_with(1), &body, "gate-fires", "r");
        assert_eq!(body, before);
        assert!(!claim("gate-fires"), "a run was started for this session");
    }

    fn scored(index: usize, score: f64) -> Scored {
        Scored {
            index,
            messages: 3,
            tokens: 100,
            score,
            head: format!("request {index}"),
            round: false,
        }
    }

    /// The percentile rule, not a fixed cutoff: whether every score is high
    /// or every score is low, the lowest quarter is stubbed.
    #[test]
    fn stub_selection_is_the_lowest_quarter_whatever_the_scale() {
        let high: Vec<Scored> = [0.9, 0.6, 0.75, 0.8, 0.7, 0.85, 0.65, 0.95]
            .into_iter()
            .enumerate()
            .map(|(i, s)| scored(i, s))
            .collect();
        assert_eq!(select_stubs(&high), vec![1, 6]);
        let low: Vec<Scored> = [0.2, 0.1, 0.15, 0.29, 0.05, 0.25, 0.12, 0.3]
            .into_iter()
            .enumerate()
            .map(|(i, s)| scored(i, s))
            .collect();
        assert_eq!(select_stubs(&low), vec![1, 4]);
        let few: Vec<Scored> = [0.1, 0.2, 0.3]
            .into_iter()
            .enumerate()
            .map(|(i, s)| scored(i, s))
            .collect();
        assert!(select_stubs(&few).is_empty());
    }

    /// A stubbed exchange becomes one user message carrying a pointer; the
    /// original lands in the store under the hash the pointer names; pairs
    /// stay whole because only whole exchanges move.
    #[test]
    fn stub_rewrite_stashes_whole_exchanges_and_keeps_pairs() {
        use headroom_core::ccr::InMemoryCcrStore;
        let msgs = transcript();
        let ex = units(&msgs);
        assert_eq!(ex.len(), 10);
        // The lowest quarter of four scored is the single lowest: exchange 2.
        let judged: Vec<Scored> = [(1, 0.8), (2, 0.5), (3, 0.7), (4, 0.6)]
            .into_iter()
            .map(|(i, s)| scored(i, s))
            .collect();
        let base_len = ex[ex.len() - PROTECT_TAIL].first;
        let store = InMemoryCcrStore::new();
        let (base, stubbed, _) = rewrite_prefix(&msgs, &ex, &judged, base_len, &store);
        assert_eq!(stubbed, 1);
        // Exchange 2 (messages 6..9) became one message; the rest verbatim.
        assert_eq!(base.len(), base_len - 2);
        let stub = base[6].get("content").and_then(Value::as_str).unwrap();
        assert!(stub.contains("<<ctx:"), "stub carries a pointer: {stub}");
        assert!(stub.contains("please do the thing number 2 in the parser"));
        assert_eq!(base[6].get("role").and_then(Value::as_str), Some("user"));
        let hash = stub
            .split("<<ctx:")
            .nth(1)
            .and_then(|s| s.split(">>").next())
            .unwrap();
        let original = store.get(hash).expect("original is retrievable");
        assert!(original.contains("please do the thing number 2 in the parser"));
        // Every tool_result still answers a tool_use in the same base.
        let (uses, results) = tool_ids(&base);
        assert_eq!(uses, results);
        assert!(!uses.contains(&"t2"));
    }

    /// Sorted `tool_use` ids and `tool_result` ids across `msgs`.
    fn tool_ids(msgs: &[Value]) -> (Vec<&str>, Vec<&str>) {
        let mut uses = Vec::new();
        let mut results = Vec::new();
        for m in msgs {
            if let Some(blocks) = m.get("content").and_then(Value::as_array) {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("tool_use") => uses.push(b.get("id").and_then(Value::as_str).unwrap()),
                        Some("tool_result") => {
                            results.push(b.get("tool_use_id").and_then(Value::as_str).unwrap());
                        }
                        _ => {}
                    }
                }
            }
        }
        uses.sort_unstable();
        results.sort_unstable();
        (uses, results)
    }

    /// One typed request, then `rounds` tool round trips, then a final reply:
    /// the long autonomous run that reaches the window.
    fn autonomous_run(rounds: usize) -> Vec<Value> {
        let mut m = vec![user("fix the lexer bug and run the parser tests")];
        for i in 0..rounds {
            m.push(assistant_tool(&format!("t{i}")));
            m.push(tool_result(&format!("t{i}"), "ok"));
        }
        m.push(json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}));
        m
    }

    /// A one-exchange run splits into its rounds: the first carries the typed
    /// request, each later one starts at an assistant turn and ends with the
    /// results that answer it, and the final reply is a round of its own.
    #[test]
    fn a_long_run_splits_into_tool_rounds() {
        let msgs = autonomous_run(12);
        let ex = units(&msgs);
        assert_eq!(ex.len(), 13, "first round, 11 later rounds, the reply");
        assert_eq!((ex[0].first, ex[0].end, ex[0].opens), (0, 3, true));
        assert_eq!((ex[1].first, ex[1].end, ex[1].opens), (3, 5, false));
        assert_eq!((ex[12].first, ex[12].end), (25, 26));
        assert!(ex.iter().all(|u| u.typed.starts_with("fix the lexer bug")));
        assert_eq!(ex[1].head, "tools Read: reading");
        assert!(ex[1].text.starts_with("request: fix the lexer bug"));
        for u in &ex {
            let (uses, results) = tool_ids(&msgs[u.first..u.end]);
            assert_eq!(
                uses, results,
                "round {}..{} keeps its pairs",
                u.first, u.end
            );
        }
        let task = current_task(&ex).unwrap();
        assert_eq!(task, 0, "the task is the opening round");
        assert_eq!(candidates(&ex, task), (1..8).collect::<Vec<_>>());
    }

    /// An exchange before the task stays whole; the task's own splits. A
    /// short "continue" after the task splits too: it carries on the same
    /// work.
    #[test]
    fn exchanges_before_the_task_stay_whole() {
        let mut m = autonomous_run(3);
        m.extend(autonomous_run(3).into_iter().map(|mut x| {
            if x["content"] == "fix the lexer bug and run the parser tests" {
                x["content"] = json!("now port the same fix to the formatter");
            }
            x
        }));
        m.extend(autonomous_run(2).into_iter().map(|mut x| {
            if x["role"] == "user" && x["content"].is_string() {
                x["content"] = json!("continue");
            }
            x
        }));
        let ex = units(&m);
        assert_eq!((ex[0].first, ex[0].end, ex[0].opens), (0, 8, true));
        assert!(ex[1..].iter().all(|u| u.first >= 8));
        assert_eq!(ex.iter().filter(|u| u.opens).count(), 3);
        assert_eq!(
            ex.len(),
            1 + 4 + 3,
            "whole, then rounds of the task, then of 'continue'"
        );
        assert_eq!(current_task(&ex), Some(1));
    }

    fn compaction_summary() -> Value {
        user(
            "This session is being continued from a previous conversation that ran out of context.\n\nSummary:\n1. Primary Request and Intent:\n   - port the lexer fix to the formatter and run its tests\n\n2. Key Technical Concepts:\n   - the parser",
        )
    }

    /// After compaction the agent runs on with no typed message: the
    /// summary's request section is the task, and the run splits into
    /// rounds behind it.
    #[test]
    fn a_compaction_summary_is_the_task_when_nothing_is_typed_after_it() {
        let mut m = autonomous_run(12);
        m[0] = compaction_summary();
        m.insert(5, user("<task-notification>x</task-notification>"));
        let ex = units(&m);
        assert_eq!(ex.len(), 13);
        assert_eq!(
            ex[0].typed,
            "- port the lexer fix to the formatter and run its tests"
        );
        assert_eq!(current_task(&ex), Some(0));
        assert_eq!(candidates(&ex, 0).len(), 7);
    }

    /// A request typed after the summary is the task, as before.
    #[test]
    fn a_request_typed_after_the_summary_is_the_task() {
        let mut m = vec![
            compaction_summary(),
            assistant_tool("s"),
            tool_result("s", "ok"),
        ];
        m.extend(autonomous_run(3));
        let ex = units(&m);
        let task = current_task(&ex).unwrap();
        assert_eq!(ex[task].first, 3);
        assert!(ex[task].typed.starts_with("fix the lexer bug"));
        assert_eq!(
            (ex[0].first, ex[0].end),
            (0, 3),
            "the summary's exchange stays whole"
        );
    }

    #[test]
    fn a_summary_without_the_request_heading_is_the_task_whole() {
        let msg = user(
            "This session is being continued from a previous conversation. Summary: did things",
        );
        assert!(summary_task(&msg).unwrap().starts_with("This session"));
        assert_eq!(summary_task(&user("please continue the work")), None);
        assert_eq!(
            summary_task(&tool_result("a", "This session is being continued")),
            None
        );
    }

    /// A harness notice after a tool result stays in that round; the next
    /// assistant turn starts a new one.
    #[test]
    fn a_notice_between_rounds_stays_in_the_earlier_round() {
        let mut m = autonomous_run(2);
        m.insert(3, user("<task-notification>x</task-notification>"));
        let ex = units(&m);
        assert_eq!((ex[0].first, ex[0].end), (0, 4));
        assert_eq!((ex[1].first, ex[1].end), (4, 6));
    }

    /// A stubbed round becomes one user message naming its tools and the
    /// pointer; its pair goes with it, every other pair stays.
    #[test]
    fn stub_rewrite_replaces_a_round_and_keeps_pairs() {
        use headroom_core::ccr::InMemoryCcrStore;
        let msgs = autonomous_run(12);
        let ex = units(&msgs);
        let judged: Vec<Scored> = (1..8)
            .map(|i| scored(i, if i == 4 { 0.1 } else { 0.8 }))
            .collect();
        let base_len = ex[ex.len() - PROTECT_TAIL].first;
        let store = InMemoryCcrStore::new();
        let (base, stubbed, _) = rewrite_prefix(&msgs, &ex, &judged, base_len, &store);
        assert_eq!(stubbed, 1);
        assert_eq!(base.len(), base_len - 1, "a two-message round became one");
        let stub = &base[ex[4].first];
        assert_eq!(stub["role"], "user");
        let text = stub["content"].as_str().unwrap();
        assert!(text.contains("an earlier tool round"), "{text}");
        assert!(text.contains("Tool round: tools Read: reading"), "{text}");
        let hash = text
            .split("<<ctx:")
            .nth(1)
            .and_then(|s| s.split(">>").next())
            .unwrap();
        let original: Vec<Value> = serde_json::from_str(&store.get(hash).unwrap()).unwrap();
        assert_eq!(original, msgs[ex[4].first..ex[4].end]);
        let (uses, results) = tool_ids(&base);
        assert_eq!(uses, results);
        assert!(!uses.contains(&"t4"));
    }

    /// Jev reads a later round with the request it serves.
    #[tokio::test]
    async fn a_round_is_scored_with_its_request() {
        let server = jev(vec![answer(0.5)]).await;
        let ex = units(&autonomous_run(12));
        let obs = observe_units(&state_with(1), &server.uri(), &ex).await;
        assert_eq!((obs.exchanges, obs.units, obs.scored.len()), (1, 13, 7));
        assert!(obs.scored.iter().all(|s| s.round));
        for req in server.received_requests().await.unwrap() {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let state = body["state"].as_str().unwrap();
            assert!(
                state.contains("Earlier exchange:\nrequest: fix the lexer bug"),
                "{state}"
            );
        }
    }

    fn apply_state() -> AppState {
        // A trigger the tiny test body never reaches: replay works without a
        // build, and no background task starts.
        crate::test_support::test_state(move |c| c.ctx_refresh_apply_tokens = 10_000_000)
    }

    /// A stored base plus the live tail is what goes forward, byte for byte.
    #[tokio::test]
    async fn apply_replays_base_plus_live_tail() {
        let msgs = transcript();
        let base_len = 24;
        let base: Vec<Value> = msgs[..base_len].to_vec();
        let first_hash = compute_key(msgs[0].to_string().as_bytes());
        bases().lock().unwrap().insert(
            "refresh-apply-replay".to_string(),
            RefreshBase {
                base: base.clone(),
                base_len,
                first_hash,
                last_hash: message_hash(msgs.get(base_len - 1)),
                built_tokens: 0,
                last_used: Instant::now(),
            },
        );
        let mut parsed = json!({"model": "claude-muse-spark-1.3", "messages": msgs});
        assert!(apply(
            &apply_state(),
            &mut parsed,
            "refresh-apply-replay",
            "r"
        ));
        let out = parsed["messages"].as_array().unwrap();
        assert_eq!(out.len(), 30);
        assert_eq!(&out[..base_len], &base[..]);
        assert_eq!(&out[base_len..], &transcript()[base_len..]);
    }

    /// A history shorter than the base (the client cleared) or with a new
    /// first message (it compacted) drops the base and forwards untouched.
    #[tokio::test]
    async fn apply_drops_a_base_the_history_outgrew() {
        let msgs = transcript();
        let first_hash = compute_key(msgs[0].to_string().as_bytes());
        bases().lock().unwrap().insert(
            "refresh-apply-stale".to_string(),
            RefreshBase {
                base: msgs[..24].to_vec(),
                base_len: 24,
                first_hash,
                last_hash: message_hash(msgs.get(23)),
                built_tokens: 0,
                last_used: Instant::now(),
            },
        );
        let mut cleared = json!({"model": "claude-muse-spark-1.3", "messages": msgs[..6].to_vec()});
        assert!(!apply(
            &apply_state(),
            &mut cleared,
            "refresh-apply-stale",
            "r"
        ));
        assert_eq!(cleared["messages"].as_array().unwrap().len(), 6);
        assert!(bases().lock().unwrap().get("refresh-apply-stale").is_none());

        bases().lock().unwrap().insert(
            "refresh-apply-compacted".to_string(),
            RefreshBase {
                base: msgs[..24].to_vec(),
                base_len: 24,
                first_hash: "deadbeef".to_string(),
                last_hash: message_hash(msgs.get(23)),
                built_tokens: 0,
                last_used: Instant::now(),
            },
        );
        let mut compacted = json!({"model": "claude-muse-spark-1.3", "messages": msgs});
        assert!(!apply(
            &apply_state(),
            &mut compacted,
            "refresh-apply-compacted",
            "r"
        ));
        assert_eq!(compacted["messages"].as_array().unwrap().len(), 30);
    }

    /// A rewind to exactly `base_len` messages keeps the length and the
    /// first message but ends on a new request; the base must not replace
    /// it.
    #[tokio::test]
    async fn apply_drops_a_base_after_a_same_length_rewind() {
        let msgs = transcript();
        bases().lock().unwrap().insert(
            "refresh-apply-rewind".to_string(),
            RefreshBase {
                base: msgs[..24].to_vec(),
                base_len: 24,
                first_hash: compute_key(msgs[0].to_string().as_bytes()),
                last_hash: message_hash(msgs.get(23)),
                built_tokens: 0,
                last_used: Instant::now(),
            },
        );
        let mut rewound = msgs[..24].to_vec();
        rewound[23] = user("a different request after the rewind");
        let mut parsed = json!({"model": "claude-muse-spark-1.3", "messages": rewound.clone()});
        assert!(!apply(
            &apply_state(),
            &mut parsed,
            "refresh-apply-rewind",
            "r"
        ));
        assert_eq!(parsed["messages"], Value::Array(rewound));
        assert!(
            bases()
                .lock()
                .unwrap()
                .get("refresh-apply-rewind")
                .is_none()
        );
    }

    fn bare_base(built_tokens: usize, last_used: Instant) -> RefreshBase {
        RefreshBase {
            base: Vec::new(),
            base_len: 0,
            first_hash: String::new(),
            last_hash: String::new(),
            built_tokens,
            last_used,
        }
    }

    /// Idle bases go first, then the least recently used past the cap.
    #[test]
    fn prune_bases_drops_idle_then_oldest() {
        let now = Instant::now() + BASE_IDLE * 2;
        let mut map = HashMap::new();
        map.insert("idle".to_string(), bare_base(0, now - BASE_IDLE));
        for i in 0..=MAX_BASES {
            let age = Duration::from_secs((MAX_BASES - i) as u64);
            map.insert(format!("s{i}"), bare_base(0, now - age));
        }
        prune_bases(&mut map, now);
        assert_eq!(map.len(), MAX_BASES);
        assert!(!map.contains_key("idle"));
        assert!(!map.contains_key("s0"), "the oldest goes past the cap");
        assert!(map.contains_key(&format!("s{MAX_BASES}")));
    }

    /// No base: build. A base: rebuild only once the history grew by the
    /// share one build stubs.
    #[test]
    fn rebuild_waits_for_growth() {
        let base = bare_base(400_000, Instant::now());
        assert!(grew_enough(None, 1));
        assert!(!grew_enough(Some(&base), 400_000));
        assert!(!grew_enough(Some(&base), 499_999));
        assert!(grew_enough(Some(&base), 500_000));
    }

    /// Off by default: with no flag, even a stored base is never replayed.
    #[tokio::test]
    async fn apply_is_off_unless_the_flag_is_set() {
        let msgs = transcript();
        bases().lock().unwrap().insert(
            "refresh-apply-off".to_string(),
            RefreshBase {
                base: Vec::new(),
                base_len: 0,
                first_hash: compute_key(msgs[0].to_string().as_bytes()),
                last_hash: message_hash(None),
                built_tokens: 0,
                last_used: Instant::now(),
            },
        );
        let mut parsed = json!({"model": "claude-muse-spark-1.3", "messages": msgs});
        let state = crate::test_support::test_state(|_| {});
        assert!(!apply(&state, &mut parsed, "refresh-apply-off", "r"));
        assert_eq!(parsed["messages"].as_array().unwrap().len(), 30);
    }
}
