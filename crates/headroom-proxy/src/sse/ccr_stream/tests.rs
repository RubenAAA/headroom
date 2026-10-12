use super::*;

pub(super) fn feed(rw: &mut Rewriter, name: &str, data: &str) -> Vec<Bytes> {
    rw.handle(SseEvent {
        event_name: Some(name.to_string()),
        data: Bytes::from(data.to_string()),
    })
}

fn joined(chunks: &[Bytes]) -> String {
    chunks
        .iter()
        .map(|b| String::from_utf8_lossy(b).to_string())
        .collect()
}

/// A continuation is buffered JSON: it must force `stream: false` and drop
/// `stream_options`, which the Responses API rejects with 400 alongside
/// `stream: false` (`stream_options requires stream to be true`). That
/// combination is what dropped every `headroom_retrieve` continuation on
/// routed Responses models after the retrieval had already been fetched.
#[test]
fn continuation_request_drops_stream_options_with_stream() {
    let forwarded = Bytes::from(
        r#"{"model":"m","stream":true,"stream_options":{"reasoning_summary_delivery":"sequential_cutoff"},"input":[]}"#,
    );
    let out = non_streaming_continuation_request(&forwarded);
    let v: Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["stream"], json!(false));
    assert!(
        v.get("stream_options").is_none(),
        "stream_options must not survive on a non-streaming continuation"
    );
}

/// The ChatGPT Codex gateway mandates streaming; other routed backends
/// keep the buffered continuation shape.
#[test]
fn mandating_backend_keeps_stream_on_continuations() {
    let destreamed = Bytes::from(r#"{"model":"m","stream":false,"input":[]}"#);
    let codex: url::Url = "https://chatgpt.com/backend-api/codex/responses"
        .parse()
        .unwrap();
    let out = restore_stream_when_mandated(destreamed.clone(), &codex);
    let v: Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["stream"], json!(true));

    // Zen rejects a de-streamed continuation with a 403 that reads like an
    // auth failure, so it belongs in the same set as the codex gateway.
    let zen: url::Url = "https://opencode.ai/zen/v1/responses".parse().unwrap();
    let out = restore_stream_when_mandated(destreamed.clone(), &zen);
    let v: Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["stream"], json!(true));

    // Everything else still takes the de-streamed body.
    let plain: url::Url = "https://api.x.ai/v1/responses".parse().unwrap();
    let out = restore_stream_when_mandated(destreamed.clone(), &plain);
    let v: Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["stream"], json!(false));

    let garbage = Bytes::from(b"not json".to_vec());
    assert_eq!(
        restore_stream_when_mandated(garbage.clone(), &codex),
        garbage,
        "unparseable bodies pass through untouched"
    );
}

/// An Anthropic continuation streams, so the 30s headers bound stops being
/// a bound on how long the model may think.
#[test]
fn anthropic_continuation_streams() {
    let forwarded = Bytes::from(
        r#"{"model":"m","stream":true,"stream_options":{"include_usage":true},"messages":[]}"#,
    );
    let out = streamed_continuation_request(forwarded);
    let v: Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["stream"], json!(true));
    assert!(v.get("stream_options").is_none());

    let garbage = Bytes::from(b"not json".to_vec());
    assert_eq!(
        streamed_continuation_request(garbage.clone()),
        garbage,
        "unparseable bodies pass through untouched"
    );
}

/// The fold reads a streamed continuation back into a turn, and refuses a
/// stream that died partway rather than passing its first blocks off as
/// the whole answer.
#[test]
fn anthropic_stream_folds_into_a_turn() {
    let body = b"event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\",\"usage\":{\"input_tokens\":9,\"cache_read_input_tokens\":4}}}\n\n\
             event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
             event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n\
             event: content_block_stop\n\
             data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
             event: message_stop\n\
             data: {\"type\":\"message_stop\"}\n\n";
    let turn = anthropic_stream_to_turn(body).expect("a complete stream folds");
    assert_eq!(turn["content"][0]["text"], "done");
    assert_eq!(turn["stop_reason"], "end_turn");
    assert_eq!(turn["usage"]["cache_read_input_tokens"], 4);

    let errored = b"event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\"}}\n\n\
             event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"half\"}}\n\n\
             event: error\n\
             data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n";
    assert!(
        anthropic_stream_to_turn(errored).is_none(),
        "a stream that errored mid-body is not an answer"
    );

    assert!(
        anthropic_stream_to_turn(b"data: not json\n\n").is_none(),
        "a body with no Anthropic events folds to nothing"
    );

    let cut_short = b"event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\"}}\n\n\
             event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"half\"}}\n\n";
    assert!(
        anthropic_stream_to_turn(cut_short).is_none(),
        "a body that ended before message_delta has no stop_reason to trust"
    );
}

/// The whole point: the client must never see the tool it cannot run.
#[test]
fn ccr_block_events_are_suppressed() {
    let mut rw = Rewriter::new(false);
    feed(
        &mut rw,
        "message_start",
        r#"{"type":"message_start","message":{"id":"m","model":"claude","usage":{"input_tokens":10,"output_tokens":0}}}"#,
    );
    let out = feed(
        &mut rw,
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"headroom_retrieve","input":{}}}"#,
    );
    assert!(
        out.is_empty(),
        "the tool_use block must not reach the client"
    );
    let out = feed(
        &mut rw,
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"hash\":\"abc\"}"}}"#,
    );
    assert!(
        out.is_empty(),
        "its deltas must not reach the client either"
    );
    assert!(rw.saw_ccr);
}

/// Memory tools are the proxy's too, and the client cannot run them
/// either. This is the case the streaming path used to miss: the rewriter
/// was built with `false` regardless of the turn's memory context, so a
/// `memory_search` block streamed through and the client answered "No such
/// tool available".
/// The other half of the same fact, and the shape of the routed-path bug:
/// a rewriter told the turn has no memory context passes the block
/// straight through. `handle_streaming_response` built its
/// `CcrStreamContext` with a hardcoded `memory: None` on the belief that
/// routed requests never carry memory tools — they do, injected at the
/// `codex_memory_tools` site — so every Codex turn that called
/// `memory_search` reached the client, which answered "No such tool
/// available: memory_search".
#[test]
fn a_memory_block_reaches_the_client_when_the_turn_claims_no_memory() {
    const START: &str = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"memory_search","input":{}}}"#;

    let mut rw = Rewriter::new(false);
    feed(
        &mut rw,
        "message_start",
        r#"{"type":"message_start","message":{"id":"m","model":"claude","usage":{"input_tokens":10,"output_tokens":0}}}"#,
    );
    assert!(
        !feed(&mut rw, "content_block_start", START).is_empty(),
        "with no memory context the block is not ours to own, and it reaches \
             the client — which is exactly the failure this documents"
    );
}

#[test]
fn memory_block_events_are_suppressed_when_memory_is_enabled() {
    const START: &str = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"memory_search","input":{}}}"#;

    let mut rw = Rewriter::new(true);
    feed(
        &mut rw,
        "message_start",
        r#"{"type":"message_start","message":{"id":"m","model":"claude","usage":{"input_tokens":10,"output_tokens":0}}}"#,
    );
    assert!(
        feed(&mut rw, "content_block_start", START).is_empty(),
        "a memory tool_use must not reach the client"
    );
    assert!(
            feed(
                &mut rw,
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"x\"}"}}"#,
            )
            .is_empty(),
            "nor may its deltas"
        );
    assert!(rw.saw_ccr, "the turn must be marked for resolution");

    // And the flag is what does it — with memory off, the same block is an
    // ordinary tool the client owns and must pass through untouched.
    let mut rw = Rewriter::new(false);
    feed(
        &mut rw,
        "message_start",
        r#"{"type":"message_start","message":{"id":"m","model":"claude","usage":{"input_tokens":10,"output_tokens":0}}}"#,
    );
    assert!(
        !feed(&mut rw, "content_block_start", START).is_empty(),
        "with memory disabled the block belongs to the client"
    );
    assert!(!rw.saw_ccr);
}

/// A turn with no retrieval must come out the far side unchanged.
#[test]
fn ordinary_blocks_pass_through_with_their_indices() {
    let mut rw = Rewriter::new(false);
    feed(
        &mut rw,
        "message_start",
        r#"{"type":"message_start","message":{"id":"m","model":"claude","usage":{"input_tokens":10,"output_tokens":0}}}"#,
    );
    let out = feed(
        &mut rw,
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    );
    assert!(joined(&out).contains("\"index\":0"));
    let out = feed(
        &mut rw,
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
    );
    assert!(joined(&out).contains("hi"));
    assert!(!rw.saw_ccr);
}

/// Text before the retrieval keeps index 0; the block after the
/// suppressed one takes index 1, not the upstream's 2.
#[test]
fn indices_close_the_gap_left_by_a_suppressed_block() {
    let mut rw = Rewriter::new(false);
    feed(
        &mut rw,
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    );
    feed(
        &mut rw,
        "content_block_start",
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"headroom_retrieve","input":{}}}"#,
    );
    let out = feed(
        &mut rw,
        "content_block_start",
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
    );
    assert!(
        joined(&out).contains("\"index\":1"),
        "client-side numbering must stay contiguous, got {}",
        joined(&out)
    );
    assert_eq!(rw.next_client_index, 2);
}

/// Terminal events are the signal that the turn is over. They cannot go
/// out before a continuation has had its chance to add to it.
#[test]
fn terminal_events_are_withheld() {
    let mut rw = Rewriter::new(false);
    let out = feed(
        &mut rw,
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
    );
    assert!(out.is_empty());
    let out = feed(&mut rw, "message_stop", r#"{"type":"message_stop"}"#);
    assert!(out.is_empty());
    assert_eq!(rw.withheld.len(), 2);
}

/// The rebuilt shape is what the buffered CCR path expects to read.
#[test]
fn rebuild_produces_the_non_streaming_response_shape() {
    let mut rw = Rewriter::new(false);
    feed(
        &mut rw,
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-x","usage":{"input_tokens":10,"output_tokens":0}}}"#,
    );
    feed(
        &mut rw,
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    );
    feed(
        &mut rw,
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"looking"}}"#,
    );
    feed(
        &mut rw,
        "content_block_stop",
        r#"{"type":"content_block_stop","index":0}"#,
    );
    feed(
        &mut rw,
        "content_block_start",
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"headroom_retrieve","input":{}}}"#,
    );
    feed(
        &mut rw,
        "content_block_delta",
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"hash\":\"abc123\"}"}}"#,
    );
    feed(
        &mut rw,
        "content_block_stop",
        r#"{"type":"content_block_stop","index":1}"#,
    );
    feed(
        &mut rw,
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
    );

    let msg = rebuild_message(&rw.state);
    assert_eq!(msg["type"], "message");
    assert_eq!(msg["id"], "msg_1");
    assert_eq!(msg["stop_reason"], "tool_use");
    let content = msg["content"].as_array().expect("content array");
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["text"], "looking");
    assert_eq!(content[1]["name"], CCR_TOOL_NAME);
    // The fragment has to survive as a real object, or the store lookup
    // has no hash to look up.
    assert_eq!(content[1]["input"]["hash"], "abc123");
}

/// Synthesised events must be the shape a client already knows how to
/// parse, and must continue the numbering rather than restart it.
#[test]
fn synthesized_blocks_continue_the_client_numbering() {
    let content = vec![json!({"type": "text", "text": "the answer"})];
    let out = synthesize_blocks(&content, 3, &[]);
    let text = joined(&out);
    assert!(text.contains("event: content_block_start"));
    assert!(text.contains("\"index\":3"));
    assert!(text.contains("\"type\":\"text_delta\""));
    assert!(text.contains("the answer"));
    assert!(text.contains("event: content_block_stop"));
    // The block opens empty and is filled by the delta, so the text must
    // appear exactly once — on the delta, not on the start event.
    assert_eq!(
        text.matches("the answer").count(),
        1,
        "content duplicated across start and delta: {text}"
    );
}

/// The orphan seed request, pinned per provider shape: the assistant turn
/// must land where each upstream reads conversation history, and the
/// sentinel-wrapped Responses items must stay wrapped.
#[test]
fn orphan_seed_request_shapes() {
    // Anthropic/chat: messages[] of role/content objects.
    let mut req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let seed = orphan_seed_request(
        &mut req,
        "messages",
        json!({"role": "assistant", "content": "I'll check."}),
    )
    .expect("messages array present");
    let back: Value = serde_json::from_slice(&seed).unwrap();
    assert_eq!(
        back["messages"],
        json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "I'll check."},
            {"role": "user", "content": ORPHAN_NUDGE},
        ])
    );
    // Responses: assistant output items splice through the sentinel, the
    // nudge is a plain user message item.
    let mut req = json!({"model": "x", "input": [
        {"_openai_responses_input_items": [{"type": "message", "role": "user", "content": "hi"}]},
    ]});
    let seed = orphan_seed_request(
        &mut req,
        "input",
        json!({"_openai_responses_input_items": [{"type": "output_text", "text": "I'll check."}]}),
    )
    .expect("input array present");
    let back: Value = serde_json::from_slice(&seed).unwrap();
    assert_eq!(
        back["input"],
        json!([
            {"_openai_responses_input_items": [{"type": "message", "role": "user", "content": "hi"}]},
            {"type": "output_text", "text": "I'll check."},
            {"type": "message", "role": "user", "content": ORPHAN_NUDGE},
        ])
    );
    // No continuation array: no seed, the turn streams as-is.
    let mut req = json!({"model": "x"});
    assert!(
        orphan_seed_request(&mut req, "messages", json!({})).is_none(),
        "without messages[] there is nothing to append to"
    );
}

/// The splice's filter, run the way the stream runs it.
fn spliceable(content: Vec<Value>, live: &[Value]) -> Vec<Value> {
    content
        .into_iter()
        .filter(|b| drop_reason(b, false, false, live).is_none())
        .collect()
}

/// The 2026-08-20 failure: three memory continuation rounds, the last
/// tool call left unresolved and dropped, and `stop_reason: tool_use`
/// forwarded regardless. Every client turn shaped like this died with
/// "the model's tool call could not be parsed (retry also failed)".
#[test]
fn a_dropped_tool_call_must_not_leave_stop_reason_claiming_one() {
    assert!(stop_reason_overclaims_tool_call(Some("tool_use"), false));
}

#[test]
fn a_surviving_tool_call_keeps_its_stop_reason() {
    assert!(!stop_reason_overclaims_tool_call(Some("tool_use"), true));
}

#[test]
fn an_ordinary_turn_is_left_alone() {
    assert!(!stop_reason_overclaims_tool_call(Some("end_turn"), false));
    assert!(!stop_reason_overclaims_tool_call(None, false));
}

/// The client-facing half of the same decision: a tool block the client
/// received counts, a proxy-owned one it never saw does not.
#[test]
fn only_tool_blocks_the_client_receives_count() {
    let mut rw = Rewriter::new(true);
    feed(
        &mut rw,
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_1","name":"memory_search","input":{}}}"#,
    );
    assert!(
        !rw.client_saw_tool_use,
        "a suppressed memory tool never reaches the client"
    );

    feed(
        &mut rw,
        "content_block_start",
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_2","name":"Bash","input":{}}}"#,
    );
    assert!(rw.client_saw_tool_use, "a client tool call does");
}

/// Anthropic signs a `thinking` block against the request that produced it.
/// The continuation is a different request, so forwarding its reasoning
/// hands the client a signature that cannot verify in the conversation it
/// replays — and the next turn comes back 400 "thinking or
/// redacted_thinking blocks in the latest assistant message cannot be
/// modified".
#[test]
fn continuation_thinking_never_reaches_the_client() {
    let live = vec![json!({"type": "thinking", "thinking": "mine", "signature": "sig-a"})];
    let resolved = vec![
        json!({"type": "thinking", "thinking": "theirs", "signature": "sig-b"}),
        json!({"type": "redacted_thinking", "data": "opaque"}),
        json!({"type": "text", "text": "the answer"}),
    ];
    let emit = spliceable(resolved, &live);
    assert_eq!(emit.len(), 1, "only the text survives: {emit:?}");
    assert_eq!(emit[0]["text"], "the answer");
}

/// When the continuation cannot run — a `headroom_retrieve` mixed with a
/// client tool call — the handler returns the turn unchanged. Every block
/// in it has already gone out live, so splicing it again duplicates the
/// turn and repeats a `tool_use` id the client is already acting on.
#[test]
fn blocks_already_streamed_are_not_sent_twice() {
    let live = vec![
        json!({"type": "text", "text": "looking"}),
        json!({"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "a"}}),
    ];
    let resolved = vec![
        json!({"type": "text", "text": "looking"}),
        // Rebuilt from deltas, so the input object need not match byte for
        // byte — the id is what makes it the same block.
        json!({"type": "tool_use", "id": "t1", "name": "Read", "input": {}}),
        json!({"type": "tool_use", "id": "ccr1", "name": CCR_TOOL_NAME, "input": {}}),
    ];
    assert!(
        spliceable(resolved, &live).is_empty(),
        "the client already has this turn"
    );
}

/// A genuinely new block from the continuation still gets through — the
/// filter must not swallow the answer the retrieval was run to produce.
#[test]
fn a_new_continuation_block_still_reaches_the_client() {
    let live = vec![json!({"type": "text", "text": "looking"})];
    let resolved = vec![
        json!({"type": "text", "text": "looking"}),
        json!({"type": "text", "text": "found it"}),
    ];
    let emit = spliceable(resolved, &live);
    assert_eq!(emit.len(), 1);
    assert_eq!(emit[0]["text"], "found it");
}

#[test]
fn synthesized_tool_use_carries_its_input_as_a_fragment() {
    let content = vec![json!({
        "type": "tool_use", "id": "t9", "name": "Read", "input": {"file_path": "/x"}
    })];
    let text = joined(&synthesize_blocks(&content, 0, &[]));
    assert!(text.contains("input_json_delta"));
    assert!(text.contains("file_path"));
}

#[test]
fn terminal_carries_the_final_rounds_usage() {
    let msg = json!({
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 900, "output_tokens": 40},
    });
    let text = joined(&synthesize_terminal(&msg, None));
    assert!(text.contains("event: message_delta"));
    assert!(text.contains("end_turn"));
    assert!(text.contains("900"));
    assert!(text.contains("event: message_stop"));
}
