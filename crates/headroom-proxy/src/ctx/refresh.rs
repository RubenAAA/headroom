//! Jev context refresh, observe mode.
//!
//! Spark sessions grow to the model's window and then fail with a generic 400.
//! The idea (`docs/notes/ideas/jev-context-refresh.md`) is to ask Jev which
//! earlier exchanges the current task still needs and stub the rest. Nothing
//! here changes a request: past `--ctx-refresh-observe-tokens` a background
//! task scores the history and logs what a refresh would have stubbed, so the
//! choices can be read before anything is applied.
//!
//! The unit is an exchange: one typed user message and everything up to the
//! next. On the 1M-token session of 2026-10-01, tool results were 20% of the
//! bytes and user-role text 43%, so scoring tool results alone would not have
//! shrunk it. Whole exchanges also never split a `tool_use`/`tool_result` pair.
//!
//! The free Jev endpoint rate-limits (429 after about 140 calls in a few
//! minutes), so a run scores at most [`MAX_SCORED`] exchanges, three at a time,
//! and a session is observed at most once per [`COOLDOWN`].

use crate::proxy::AppState;
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const JEV_MODEL: &str = "jev-1.13-free";
const QUESTION: &str =
    "Does the current task need the details of this earlier exchange to be done correctly?";
/// The 1M-token session of 2026-10-01 was 4.5 MB on the wire.
const BYTES_PER_TOKEN: usize = 4;
/// Exchanges never scored: the opening one, and the newest few.
const PROTECT_HEAD: usize = 1;
const PROTECT_TAIL: usize = 5;
const MAX_SCORED: usize = 100;
const CONCURRENCY: usize = 3;
const COOLDOWN: Duration = Duration::from_secs(30 * 60);
const RATE_LIMIT_RETRIES: u32 = 5;
const JEV_TIMEOUT: Duration = Duration::from_secs(30);
const TASK_CHARS: usize = 1500;
const EXCHANGE_CHARS: usize = 5000;
const PART_CHARS: usize = 400;
/// Scores under each of these would be stubbed; the log reports all three.
const THRESHOLDS: [f64; 3] = [0.3, 0.4, 0.5];
/// User text that is not a request from the person typing.
const NOT_TYPED: [&str; 5] = [
    "<task-notification>",
    "Stop hook feedback",
    "[Request interrupted",
    "<local-command",
    "This session is being continued",
];

/// One typed user message and the messages that answer it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Exchange {
    pub first: usize,
    pub end: usize,
    pub typed: String,
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
/// a harness notice.
fn typed_text(msg: &Value) -> Option<String> {
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
    let (typed, _) = crate::cache_stabilization::ephemeral_spans::split_ephemeral_spans(&raw);
    let typed = typed.trim();
    if typed.is_empty() || NOT_TYPED.iter().any(|p| typed.starts_with(p)) {
        return None;
    }
    Some(typed.to_string())
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

/// Split a transcript into exchanges. Messages before the first typed one
/// belong to the first exchange.
pub(crate) fn exchanges(messages: &[Value]) -> Vec<Exchange> {
    let mut starts: Vec<(usize, String)> = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if let Some(t) = typed_text(m) {
            starts.push((i, t));
        }
    }
    if starts.is_empty() {
        return Vec::new();
    }
    starts[0].0 = 0;
    let mut out = Vec::with_capacity(starts.len());
    for (k, (first, typed)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(messages.len(), |(next, _)| *next);
        let slice = &messages[*first..end];
        let mut text = String::new();
        for m in slice {
            render_message(m, &mut text);
        }
        out.push(Exchange {
            first: *first,
            end,
            typed: typed.clone(),
            bytes: slice.iter().map(|m| m.to_string().len()).sum(),
            text: cap_exchange(&text),
        });
    }
    out
}

/// The last request that says something: not a stray "continue" or "ok".
fn current_task(ex: &[Exchange]) -> Option<usize> {
    ex.iter().rposition(|e| e.typed.chars().count() >= 20)
}

/// What gets scored: everything but the opening, the newest few and the task's
/// own exchange; the largest first, since they hold the savings.
fn candidates(ex: &[Exchange], task: usize) -> Vec<usize> {
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
}

#[derive(Debug, Default)]
pub(crate) struct Observation {
    pub exchanges: usize,
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
pub(crate) async fn observe_exchanges(state: &AppState, url: &str, ex: &[Exchange]) -> Observation {
    let mut obs = Observation {
        exchanges: ex.len(),
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
                head: truncate_chars(&ex[i].typed.replace('\n', " "), 100),
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
            "Jev scored an earlier exchange against the current task"
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
        scored = obs.scored.len(),
        errors = obs.errors,
        stub_lt_0_3_exchanges = n3,
        stub_lt_0_3_tokens = t3,
        stub_lt_0_4_exchanges = n4,
        stub_lt_0_4_tokens = t4,
        stub_lt_0_5_exchanges = n5,
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
        let ex = exchanges(&msgs);
        let obs = observe_exchanges(&state, &url, &ex).await;
        log_observation(&session_key, &request_id, total_tokens, &obs);
    });
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
        let ex = exchanges(&transcript());
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
        let ex = exchanges(&m);
        assert_eq!(ex.len(), 2);
        assert_eq!(ex[0].typed, "first real request", "reminder span is lifted");
        assert_eq!(
            ex[0].end, 6,
            "notices stay inside the exchange they interrupt"
        );
    }

    #[test]
    fn messages_before_the_first_typed_one_join_the_first_exchange() {
        let mut m = vec![assistant_tool("a"), tool_result("a", "ok")];
        m.push(user("then a typed request that is long enough"));
        let ex = exchanges(&m);
        assert_eq!(ex.len(), 1);
        assert_eq!((ex[0].first, ex[0].end), (0, 3));
    }

    #[test]
    fn no_typed_message_means_no_exchanges() {
        assert!(exchanges(&[assistant_tool("a"), tool_result("a", "ok")]).is_empty());
        assert!(exchanges(&[]).is_empty());
    }

    #[test]
    fn the_opening_the_tail_and_the_task_are_never_scored() {
        let ex = exchanges(&transcript());
        let task = current_task(&ex).unwrap();
        assert_eq!(task, 9);
        let c = candidates(&ex, task);
        assert_eq!(c, vec![1, 2, 3, 4], "0 is the opening; 5..=9 are the tail");
    }

    #[test]
    fn a_short_last_message_is_not_the_task() {
        let mut m = transcript();
        m.push(user("continue"));
        let ex = exchanges(&m);
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
        let ex = exchanges(&transcript());
        let obs = observe_exchanges(&state_with(1), &server.uri(), &ex).await;
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
        let ex = exchanges(&transcript());
        let obs = observe_exchanges(&state_with(1), &server.uri(), &ex).await;
        assert_eq!(
            (obs.scored.len(), obs.errors),
            (4, 0),
            "two 429s then success"
        );

        let always = jev(vec![ResponseTemplate::new(429)]).await;
        let obs = observe_exchanges(&state_with(1), &always.uri(), &ex).await;
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
            reqwest::Client::builder()
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
        let ex = exchanges(&transcript());
        let obs = observe_exchanges(&state, "http://jev.invalid/systemone", &ex).await;
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
        let lane = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(limited.uri()).unwrap())
            .build()
            .unwrap();
        let pool = std::sync::Arc::new(crate::proxy::ProviderEgressPool::new(
            vec![lane.clone(), lane],
            vec!["proxy-a".to_string(), "proxy-b".to_string()],
        ));
        let mut state = state_with(1);
        state.zen_egresses = Some(pool);
        let ex = exchanges(&transcript());
        let obs = observe_exchanges(&state, "http://jev.invalid/systemone", &ex).await;
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
        let ex = exchanges(&transcript());
        let obs = observe_exchanges(&state_with(1), &server.uri(), &ex).await;
        assert_eq!((obs.scored.len(), obs.errors), (1, 3));
        assert!((obs.scored[0].score - 0.9).abs() < 1e-9);
    }

    #[tokio::test]
    async fn nothing_is_asked_without_a_task() {
        let server = jev(vec![answer(0.5)]).await;
        let obs = observe_exchanges(&state_with(1), &server.uri(), &[]).await;
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
}
