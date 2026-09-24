//! usage_observer::first_turn — split from usage_observer.rs (pure move, no logic change).
use super::*;

/// What the request side knows about a turn that may turn out to be the first
/// completed one under its conversation key. Computed once in the handler,
/// where the parsed body is, and parked here until the usage arrives — see
/// [`UsageObserver::note_first_turn_context`].
///
/// The D0 diagnostic (`first-turn-write-sharing.md`) extends this with the
/// cache-key controls that live outside system/tools/messages: they are only
/// visible on the request path, and the response side must not guess them.
#[derive(Debug, Clone, Default)]
pub struct FirstTurnContext {
    /// Messages the client sent, before any compression.
    pub msgs: usize,
    /// Canonical hash of message 0, for the identical-prompt fan-out check.
    pub message_zero_hash: Option<String>,
    /// Message 0 carries Claude Code's compaction summary marker.
    pub compaction_restart: bool,
    pub model: Option<String>,
    /// Top-level `tool_choice`: `auto` / `any` / `tool:<name>` / `none`.
    /// A named tool choice is part of the provider cache key.
    pub tool_choice: Option<String>,
    /// Top-level `thinking`: `absent` / `disabled` / `enabled:<budget>`.
    pub thinking: Option<String>,
    /// Top-level `effort`, when the client sends one. Short scalar only.
    pub effort: Option<String>,
    /// Message 0 carries an image block. Images are message content, so one
    /// here only affects the message span — recorded so the diagnostic can
    /// rule it in or out rather than assume.
    pub images_in_m0: bool,
    /// Message 0 opens with the shared `<system-reminder>` scaffolding run.
    pub opens_with_scaffolding: bool,
    /// Byte size of that leading scaffolding run in message 0.
    pub m0_scaffold_bytes: u64,
    /// Byte size of the rest of message 0's text (task, recall, summary).
    pub m0_rest_bytes: u64,
}

/// Outcome of the cross-session prefix adoption path: the replay store found a
/// donor tracker under another session key whose originals prefix-match this
/// request. Whether the donor's bytes reached the wire is read off the replay
/// evidence when the event is emitted.
#[derive(Debug, Clone)]
pub struct PrefixAdoption {
    pub donor_session_key_hash: String,
}

/// Compute [`FirstTurnContext`] from the client's parsed body, once, on the
/// request path.
pub fn first_turn_context(parsed: &serde_json::Value) -> FirstTurnContext {
    use sha2::{Digest, Sha256};
    let messages = parsed
        .get("messages")
        .and_then(|m| m.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let message_zero_hash = messages.first().map(|m| {
        let canonical = super::super::prefix_replay::canonicalize_for_prefix_compare(m);
        // Stream into the digest instead of materializing the
        // serialized copy (identical input bytes, no buffer).
        let mut hasher = Sha256::new();
        let _ = serde_json::to_writer(DigestSink(&mut hasher), &canonical);
        hex16(hasher.finalize().as_slice())
    });
    let compaction_restart = crate::ctx::identity::first_user_message_text(parsed)
        .is_some_and(|t| crate::ctx::identity::has_compaction_marker(&t));
    FirstTurnContext {
        msgs: messages.len(),
        message_zero_hash,
        compaction_restart,
        model: parsed
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        tool_choice: describe_tool_choice(parsed),
        thinking: describe_thinking(parsed),
        effort: describe_effort(parsed),
        images_in_m0: message_zero_has_image(messages),
        opens_with_scaffolding: super::super::prefix_replay::opens_with_scaffolding(messages),
        m0_scaffold_bytes: message_zero_composition(messages).0,
        m0_rest_bytes: message_zero_composition(messages).1,
    }
}

/// Top-level `tool_choice` in a bounded vocabulary. `None` when absent or an
/// unexpected shape — the diagnostic must never mislabel a novel client.
fn describe_tool_choice(parsed: &serde_json::Value) -> Option<String> {
    const MAX_NAME: usize = 32;
    let choice = parsed.get("tool_choice")?;
    let kind = choice.get("type").and_then(|v| v.as_str())?;
    match kind {
        "auto" | "any" | "none" => Some(kind.to_string()),
        "tool" => {
            let name: String = choice
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .chars()
                .take(MAX_NAME)
                .collect();
            Some(format!("tool:{name}"))
        }
        _ => None,
    }
}

/// Top-level `thinking` in a bounded vocabulary: absent, disabled, or the
/// enabled budget that sizes the thinking span.
fn describe_thinking(parsed: &serde_json::Value) -> Option<String> {
    let thinking = parsed.get("thinking")?;
    match thinking.get("type").and_then(|v| v.as_str()) {
        None => None,
        Some("disabled") => Some("disabled".to_string()),
        Some("enabled") => Some(format!(
            "enabled:{}",
            thinking
                .get("budget_tokens")
                .and_then(|v| v.as_u64())
                .map(|b| b.to_string())
                .unwrap_or_else(|| "?".to_string())
        )),
        _ => None,
    }
}

/// Top-level `effort`, when present. Short scalar only; anything else is
/// `None` rather than a lossy rendering.
fn describe_effort(parsed: &serde_json::Value) -> Option<String> {
    const MAX: usize = 16;
    match parsed.get("effort") {
        Some(serde_json::Value::String(s)) => Some(s.chars().take(MAX).collect()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Whether message 0 carries an image block. First message only: deeper
/// history cannot affect the sys/tools checkpoint match the diagnostic is
/// about, and walking it on every request would price the instrumentation
/// against long conversations for no signal.
fn message_zero_has_image(messages: &[serde_json::Value]) -> bool {
    messages
        .first()
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .is_some_and(|blocks| {
            blocks.iter().any(|b| {
                b.get("type").and_then(|v| v.as_str()) == Some("image")
                    || b.get("source")
                        .and_then(|s| s.get("type"))
                        .and_then(|v| v.as_str())
                        == Some("base64")
            })
        })
}

/// Byte split of message 0's text into the leading shared-scaffolding run and
/// everything after it. Sizes only, never content: the diagnostic needs to
/// know how much of the opener another session could share, not what it says.
fn message_zero_composition(messages: &[serde_json::Value]) -> (u64, u64) {
    use super::super::ephemeral_spans::is_ephemeral_client_block;
    let Some(first) = messages.first() else {
        return (0, 0);
    };
    match first.get("content") {
        Some(serde_json::Value::Array(blocks)) => {
            let run = blocks
                .iter()
                .take_while(|b| is_ephemeral_client_block(b))
                .count();
            let text_len = |b: &serde_json::Value| {
                b.get("text")
                    .and_then(|t| t.as_str())
                    .map_or(0, |t| t.len() as u64)
            };
            let scaffold: u64 = blocks.iter().take(run).map(text_len).sum();
            let rest: u64 = blocks.iter().skip(run).map(text_len).sum();
            (scaffold, rest)
        }
        Some(serde_json::Value::String(text)) => {
            if super::super::ephemeral_spans::is_ephemeral_client_text(text) {
                (text.len() as u64, 0)
            } else {
                (0, text.len() as u64)
            }
        }
        _ => (0, 0),
    }
}

/// Why the first completed turn under a conversation key wrote cache. Bounded
/// vocabulary; the metric label is built from it.
///
/// Precedence, when more than one applies: compaction restart, then session
/// key drift, then identical-prompt fan-out, then a fresh session, and
/// `arrived_with_history` when nothing else explains a turn that carried more
/// than an opener.
pub fn first_turn_reason(
    ctx: &FirstTurnContext,
    adoption: Option<&PrefixAdoption>,
    opener_seen_elsewhere: bool,
) -> &'static str {
    if ctx.compaction_restart {
        "compaction_restart"
    } else if adoption.is_some() {
        "session_key_drift"
    } else if ctx.msgs <= 2 && opener_seen_elsewhere {
        "identical_prompt_fanout"
    } else if ctx.msgs <= 2 {
        "fresh_session"
    } else {
        "arrived_with_history"
    }
}

#[cfg(test)]
mod first_turn_attribution_tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};

    #[derive(Default)]
    struct Captured {
        lines: Vec<String>,
    }

    struct CaptureFields(Arc<StdMutex<Captured>>);

    impl<S: tracing::Subscriber> Layer<S> for CaptureFields {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            struct V(String);
            impl tracing::field::Visit for V {
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    self.0.push_str(&format!("{}={:?} ", f.name(), v));
                }
                fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                    self.0.push_str(&format!("{}={} ", f.name(), v));
                }
            }
            let mut v = V(String::new());
            event.record(&mut v);
            self.0.lock().unwrap().lines.push(v.0);
        }
    }

    fn ctx(msgs: usize, hash: &str, compaction: bool) -> FirstTurnContext {
        FirstTurnContext {
            msgs,
            message_zero_hash: Some(hash.into()),
            compaction_restart: compaction,
            model: Some("claude-sonnet-5".into()),
            ..Default::default()
        }
    }

    /// Run `f` against a fresh observer and return every
    /// `first_turn_write_observed` line it emitted.
    fn first_turn_lines(f: impl FnOnce(&UsageObserver)) -> Vec<String> {
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));
        tracing::subscriber::with_default(sub, || f(&UsageObserver::new()));
        let lines = cap.lock().unwrap().lines.clone();
        lines
            .into_iter()
            .filter(|l| l.contains("first_turn_write_observed"))
            .collect()
    }

    fn one_turn(obs: &UsageObserver, rid: &str, conv: &str, c: FirstTurnContext, creation: u64) {
        obs.begin_request(rid, conv.into(), Some(&format!("sess-{conv}")), None, None);
        obs.note_first_turn_context(rid, c);
        obs.complete(rid, 300, 0, creation, None);
    }

    fn fp_msgs(stable_msgs: usize) -> PrefixFingerprint {
        PrefixFingerprint {
            head: "head".into(),
            head_model: "m".into(),
            head_system: "s".into(),
            head_tools: "t".into(),
            body: "body".into(),
            stable: format!("stable-{stable_msgs}"),
            stable_msgs,
        }
    }

    /// Run `f` against a fresh observer and return every
    /// `first_turn_prefix_diagnostic` line it emitted.
    fn diagnostic_lines(f: impl FnOnce(&UsageObserver)) -> Vec<String> {
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));
        tracing::subscriber::with_default(sub, || f(&UsageObserver::new()));
        let lines = cap.lock().unwrap().lines.clone();
        lines
            .into_iter()
            .filter(|l| l.contains("first_turn_prefix_diagnostic"))
            .collect()
    }

    #[test]
    fn reason_precedence() {
        let adoption = PrefixAdoption {
            donor_session_key_hash: "donor".into(),
        };
        // Compaction beats everything, even a found donor and a fan-out hit.
        assert_eq!(
            first_turn_reason(&ctx(2, "h", true), Some(&adoption), true),
            "compaction_restart"
        );
        assert_eq!(
            first_turn_reason(&ctx(2, "h", false), Some(&adoption), true),
            "session_key_drift"
        );
        assert_eq!(
            first_turn_reason(&ctx(2, "h", false), None, true),
            "identical_prompt_fanout"
        );
        assert_eq!(
            first_turn_reason(&ctx(2, "h", false), None, false),
            "fresh_session"
        );
        assert_eq!(
            first_turn_reason(&ctx(9, "h", false), None, false),
            "arrived_with_history"
        );
        // Fan-out only means something for an opener; a long history that
        // happens to share message 0 is still new content.
        assert_eq!(
            first_turn_reason(&ctx(9, "h", false), None, true),
            "arrived_with_history"
        );
    }

    #[test]
    fn diagnostic_fires_for_every_first_turn_classification() {
        let lines = diagnostic_lines(|obs| {
            // A tracked stream exists, but the short turn matches none of
            // them: FirstTurn with streams_tracked == 1, the shape
            // `first_turn_write_observed` never sees.
            obs.begin_request("d1", "conv-d".into(), None, None, Some(fp_msgs(40)));
            obs.note_first_turn_context("d1", ctx(40, "h40", false));
            obs.complete("d1", 300, 0, 50_000, None);
            obs.begin_request("d2", "conv-d".into(), None, None, Some(fp_msgs(2)));
            obs.note_first_turn_context("d2", ctx(1, "h1", false));
            obs.note_billed_totals("d2", 310, 5_000, 52_000);
            obs.complete("d2", 300, 0, 50_000, None);
            // A continuation of the tracked stream: healthy, no diagnostic.
            obs.begin_request("d3", "conv-d".into(), None, None, Some(fp_msgs(41)));
            obs.note_first_turn_context("d3", ctx(41, "h41", false));
            obs.complete("d3", 300, 40_000, 5_000, None);
        });
        assert_eq!(lines.len(), 2, "{lines:?}");
        let d2 = lines
            .iter()
            .find(|l| l.contains("request_id=d2"))
            .expect("tracked first turn measured");
        for field in [
            "msgs=1 ",
            "outer_cache_write=50000",
            "rounds_input_tokens=10",
            "rounds_cache_read=5000",
            "rounds_cache_write=2000",
            "outer_write_5m=-1",
            "tool_choice=absent",
            "thinking=absent",
            "effort=absent",
            "opens_with_scaffolding=false",
        ] {
            assert!(d2.contains(field), "{field} missing: {d2}");
        }
        assert!(
            !lines.iter().any(|l| l.contains("request_id=d3")),
            "healthy turns emit nothing: {lines:?}"
        );
    }

    #[test]
    fn fresh_session_write_is_attributed() {
        let lines =
            first_turn_lines(|obs| one_turn(obs, "r1", "conv-a", ctx(1, "h1", false), 9_000));
        assert_eq!(lines.len(), 1, "{lines:?}");
        let l = &lines[0];
        assert!(l.contains("attribution_reason=fresh_session"), "{l}");
        assert!(l.contains("msgs=1 "), "{l}");
        assert!(l.contains("cache_creation_input_tokens=9000"), "{l}");
        assert!(l.contains("model=claude-sonnet-5"), "{l}");
        assert!(l.contains("conversation_key=conv-a"), "{l}");
        assert!(!l.contains("adopted="), "no adoption ran: {l}");
    }

    #[test]
    fn compaction_restart_is_attributed() {
        let lines =
            first_turn_lines(|obs| one_turn(obs, "r1", "conv-a", ctx(1, "h1", true), 9_000));
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("attribution_reason=compaction_restart"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn arrived_with_history_is_attributed() {
        let lines =
            first_turn_lines(|obs| one_turn(obs, "r1", "conv-a", ctx(17, "h1", false), 9_000));
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("attribution_reason=arrived_with_history"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn a_found_donor_means_session_key_drift() {
        let lines = first_turn_lines(|obs| {
            obs.begin_request("r1", "conv-a".into(), Some("sess-a"), None, None);
            obs.note_first_turn_context("r1", ctx(17, "h1", false));
            obs.note_prefix_adoption(
                "r1",
                PrefixAdoption {
                    donor_session_key_hash: "d0n0r".into(),
                },
            );
            obs.complete("r1", 300, 0, 9_000, None);
        });
        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        assert!(l.contains("attribution_reason=session_key_drift"), "{l}");
        assert!(l.contains("adopted=false"), "{l}");
        assert!(l.contains("donor_session_key_hash=d0n0r"), "{l}");
    }

    #[test]
    fn the_same_opener_under_another_key_is_fanout() {
        let lines = first_turn_lines(|obs| {
            one_turn(obs, "r1", "conv-a", ctx(1, "same", false), 9_000);
            one_turn(obs, "r2", "conv-b", ctx(1, "same", false), 9_000);
            // A different opener is a fresh session in its own right.
            one_turn(obs, "r3", "conv-c", ctx(1, "other", false), 9_000);
        });
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].contains("attribution_reason=fresh_session"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("attribution_reason=identical_prompt_fanout"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].contains("attribution_reason=fresh_session"),
            "{}",
            lines[2]
        );
    }

    #[test]
    fn a_second_turn_under_the_key_emits_nothing() {
        let lines = first_turn_lines(|obs| {
            one_turn(obs, "r1", "conv-a", ctx(1, "h1", false), 9_000);
            // Second turn re-writes the whole prefix: a recache, not a first
            // turn, and it must not be booked here.
            one_turn(obs, "r2", "conv-a", ctx(3, "h1", false), 12_000);
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("request_id=r1"), "{}", lines[0]);
    }

    #[test]
    fn context_is_read_off_the_body() {
        let body = serde_json::json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "This session is being continued from a previous conversation. Summary:"}]},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": "go"}
            ]
        });
        let c = first_turn_context(&body);
        assert_eq!(c.msgs, 3);
        assert!(c.compaction_restart);
        assert_eq!(c.model.as_deref(), Some("claude-opus-5"));
        // The hash ignores cache_control, so two subagents whose openers
        // differ only in breakpoints still fan out together.
        let mut with_cc = body.clone();
        with_cc["messages"][0]["content"][0]["cache_control"] =
            serde_json::json!({"type": "ephemeral"});
        assert_eq!(
            c.message_zero_hash,
            first_turn_context(&with_cc).message_zero_hash
        );
        let plain = first_turn_context(
            &serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}),
        );
        assert!(!plain.compaction_restart);
        assert_ne!(plain.message_zero_hash, c.message_zero_hash);
        assert!(
            first_turn_context(&serde_json::json!({}))
                .message_zero_hash
                .is_none()
        );
    }

    #[test]
    fn a_first_turn_that_wrote_nothing_is_silent() {
        let lines = first_turn_lines(|obs| {
            one_turn(
                obs,
                "r1",
                "conv-a",
                ctx(1, "h1", false),
                RECACHE_SLACK_TOKENS,
            );
        });
        assert!(lines.is_empty(), "{lines:?}");
    }
}
