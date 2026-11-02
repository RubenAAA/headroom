use super::*;
use serde_json::json;

fn cfg(min: usize) -> CtxOffloadConfig {
    CtxOffloadConfig {
        min_bytes: min,
        stale_margin: 0,
        stale_window: 0,
        cross_session_seed: false,
    }
}

/// A Read of `path` at `id`, already offloaded to `hash`.
fn offloaded_read(id: &str, path: &str, hash: &str) -> Vec<Value> {
    vec![
        json!({"role":"assistant","content":[
            {"type":"tool_use","id":id,"name":"Read","input":{"file_path":path}}
        ]}),
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":id,
             "content": format!("digest\n{MARKER_PREFIX}{hash}>> (900 bytes offloaded)")}
        ]}),
    ]
}

fn edit_of(id: &str, path: &str) -> Value {
    json!({"role":"assistant","content":[
        {"type":"tool_use","id":id,"name":"Edit","input":{"file_path":path}}
    ]})
}

#[test]
fn an_offloaded_read_edited_afterwards_is_reported_stale() {
    let mut msgs = offloaded_read("tu_1", "/src/a.rs", "abc123");
    msgs.push(edit_of("tu_2", "/src/a.rs"));

    let stale = stale_offloaded_reads(&msgs);

    assert_eq!(stale.get("abc123").map(String::as_str), Some("/src/a.rs"));
}

#[test]
fn a_read_never_edited_is_not_reported() {
    let msgs = offloaded_read("tu_1", "/src/a.rs", "abc123");

    assert!(stale_offloaded_reads(&msgs).is_empty());
}

#[test]
fn an_edit_before_the_read_does_not_make_it_stale() {
    let mut msgs = vec![edit_of("tu_0", "/src/a.rs")];
    msgs.extend(offloaded_read("tu_1", "/src/a.rs", "abc123"));

    assert!(stale_offloaded_reads(&msgs).is_empty());
}

#[test]
fn an_edit_to_a_different_file_does_not_make_it_stale() {
    let mut msgs = offloaded_read("tu_1", "/src/a.rs", "abc123");
    msgs.push(edit_of("tu_2", "/src/b.rs"));

    assert!(stale_offloaded_reads(&msgs).is_empty());
}

/// Only blocks we actually offloaded have a hash to warn about.
#[test]
fn a_stale_read_that_was_never_offloaded_has_no_hash() {
    let msgs = vec![
        json!({"role":"assistant","content":[
            {"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"/src/a.rs"}}
        ]}),
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"tu_1","content":"the whole file, verbatim"}
        ]}),
        edit_of("tu_2", "/src/a.rs"),
    ];

    assert!(stale_offloaded_reads(&msgs).is_empty());
}

#[test]
fn multi_edit_counts_as_an_edit() {
    let mut msgs = offloaded_read("tu_1", "/src/a.rs", "abc123");
    msgs.push(json!({"role":"assistant","content":[
        {"type":"tool_use","id":"tu_2","name":"MultiEdit","input":{"file_path":"/src/a.rs"}}
    ]}));

    assert!(stale_offloaded_reads(&msgs).contains_key("abc123"));
}

#[test]
fn the_warning_names_the_file_and_tells_the_model_to_re_read() {
    let w = stale_read_warning("/src/a.rs");

    assert!(w.contains("/src/a.rs"));
    assert!(w.contains("BEFORE"));
}

/// A tool_result body whose content is `body`, paired to a bash tool_use.
fn req(body: &str) -> Value {
    json!({
        "model": "claude-3-5-sonnet-20241022",
        "messages": [
            {"role":"assistant","content":[
                {"type":"tool_use","id":"tu_1","name":"Bash",
                 "input":{"command":"cat big.log"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu_1","content": body}
            ]}
        ]
    })
}

/// A screenshot tool returns its page text beside the image. Offload
/// stores only the text, so the image must stay in the block: dropped,
/// no retrieval could bring it back.
#[test]
fn offload_keeps_image_and_document_blocks_beside_the_digest() {
    let image = json!({"type":"image","source":{"type":"base64",
            "media_type":"image/png","data":"iVBORw0KGgo="}});
    let document = json!({"type":"document","source":{"type":"base64",
            "media_type":"application/pdf","data":"JVBERi0x"}});
    let mut parsed = req("");
    parsed["messages"][1]["content"][0]["content"] = json!([
        {"type":"text","text": big_body()},
        image.clone(),
        {"type":"text","text": big_body()},
        document.clone(),
    ]);
    let out = offload_anthropic_request(&mut parsed, &cfg(200), None);
    assert_eq!(out.blocks_offloaded, 1);

    let content = parsed["messages"][1]["content"][0]["content"]
        .as_array()
        .expect("array stays an array");
    let types: Vec<&str> = content.iter().filter_map(|b| b["type"].as_str()).collect();
    assert_eq!(types, ["text", "image", "document"]);
    assert_eq!(content[1], image);
    assert_eq!(content[2], document);
    assert!(content[0]["text"].as_str().unwrap().contains(MARKER_PREFIX));

    // Re-running on the digest is a no-op (idempotency).
    let again = parsed.clone();
    let out = offload_anthropic_request(&mut parsed, &cfg(200), None);
    assert_eq!(out.blocks_offloaded, 0);
    assert_eq!(parsed, again);
}

fn first_tool_result_text(parsed: &Value) -> String {
    let content = &parsed["messages"][1]["content"][0]["content"];
    tool_result_text(content).unwrap_or_default()
}

/// Every tool on the old default exclusion list offloads now, live tail
/// included. Only [`headroom_core::tool_exclusion::is_verbatim_excluded`]
/// still stops a block, and none of these are on it.
#[test]
fn the_old_exclusion_list_no_longer_holds_a_block_back() {
    for tool in ["Read", "Grep", "Glob", "Write", "Edit", "Bash"] {
        let body = big_body();
        let mut parsed = req_with_tail(&body, tool, 0);
        let config = CtxOffloadConfig {
            min_bytes: 200,
            stale_margin: 0,
            stale_window: 0,
            cross_session_seed: false,
        };
        let out = offload_anthropic_request(&mut parsed, &config, None);
        assert_eq!(out.blocks_offloaded, 1, "{tool} must offload");
    }
}

/// A conversation whose `tool_result` sits at message 1 with `tail` filler
/// messages after it, so its distance from the tail is what the test varies.
fn req_with_tail(body: &str, tool: &str, tail: usize) -> Value {
    let mut messages = vec![
        json!({"role":"assistant","content":[
            {"type":"tool_use","id":"tu_1","name":tool,"input":{"file_path":"/big.rs"}}
        ]}),
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"tu_1","content": body}
        ]}),
    ];
    for i in 0..tail {
        messages.push(json!({"role":"assistant","content":[
            {"type":"text","text":format!("step {i}")}
        ]}));
    }
    json!({"model":"claude-3-5-sonnet-20241022","messages": messages})
}

fn margin_cfg(stale_margin: usize) -> CtxOffloadConfig {
    CtxOffloadConfig {
        min_bytes: 200,
        stale_margin,
        stale_window: 0,
        cross_session_seed: false,
    }
}

fn big_body() -> String {
    "ERROR: disk full on volume /dev/sda1\n".repeat(200)
}

/// A file read close to the tail is offloaded like anything else. The
/// `--exclude-tools` list used to hold it back until `stale_margin`
/// messages had passed, on the argument that the model must not act on a
/// summary of a file it is about to edit. Offload is not a summary: the
/// digest keeps the head and `headroom_retrieve` returns the rest byte for
/// byte, verified over 10,415 round trips against the client's own re-sent
/// bytes. Holding the list here cost 4.8 points of the bill on a 7,839-turn
/// capture and 11.8 on the same capture under subscription weights.
#[test]
fn a_file_read_is_offloaded_at_any_distance_from_the_tail() {
    for tail in [1, 6, 40] {
        let body = big_body();
        let mut parsed = req_with_tail(&body, "Read", tail);
        let out = offload_anthropic_request(&mut parsed, &margin_cfg(4), None);
        assert_eq!(
            out.blocks_offloaded, 1,
            "Read at distance {tail} must offload"
        );
    }
}

/// Verbatim exclusions are not distance-sensitive: those results break when
/// their bytes change at all, so no margin reaches them.
#[test]
fn a_verbatim_excluded_tool_is_never_offloaded_however_stale() {
    let body = big_body();
    let mut parsed = req_with_tail(&body, "WebFetch", 40);
    let config = CtxOffloadConfig {
        min_bytes: 200,
        stale_margin: 4,
        stale_window: 0,
        cross_session_seed: false,
    };
    let out = offload_anthropic_request(&mut parsed, &config, None);
    assert_eq!(out.blocks_offloaded, 0, "WebFetch must stay byte-faithful");
}

/// The cache-safety half of `stale_margin`. Distance from the tail grows,
/// so a first conversion of a block inside the cached prefix has to wait for
/// a turn that is rewriting the prefix anyway.
#[test]
fn a_newly_stale_block_waits_for_a_rebuild_boundary() {
    let body = big_body();
    let mut parsed = req_with_tail(&body, "Read", 6);
    let gate = OffloadGate::new(16);
    let policy = OffloadPolicy {
        gate: &gate,
        session_key: "sess",
        rebuild_boundary: false,
    };
    let out = offload_anthropic_request(&mut parsed, &margin_cfg(4), Some(&policy));
    assert_eq!(out.blocks_offloaded, 0, "not on a steady-state turn");
    assert_eq!(out.blocks_deferred, 1, "deferred, not abandoned");
    assert!(
        out.bytes_deferred > 0,
        "the deferral records what it is holding raw, not just that it held something"
    );
    assert!(
        out.deferred_min_distance.is_some(),
        "and how far from the tail it sits, which is what prices the rewrite"
    );
    assert!(
        out.bytes_after_deepest_deferral > 0,
        "and what converting it would force rewritten"
    );
    assert_eq!(
        first_tool_result_text(&parsed),
        body,
        "prefix bytes unchanged"
    );
}

/// The other half, and the one that is easy to miss: Claude Code edits its
/// own history, so the message count can FALL and a stale block can read as
/// fresh again. Reverting the digest would bust the prefix exactly as hard
/// as converting it late, so a conversion already made has to stand.
#[test]
fn a_converted_block_stays_converted_when_the_history_shrinks() {
    let body = big_body();
    let gate = OffloadGate::new(16);
    let boundary = OffloadPolicy {
        gate: &gate,
        session_key: "sess",
        rebuild_boundary: true,
    };
    let mut deep = req_with_tail(&body, "Read", 6);
    let first = offload_anthropic_request(&mut deep, &margin_cfg(4), Some(&boundary));
    assert_eq!(first.blocks_offloaded, 1, "converts on the boundary turn");
    let digest = first_tool_result_text(&deep);

    // Same block, now one message from the tail: inside the margin, so the
    // exclusion would apply again if `prior` were not checked first.
    let mut shallow = req_with_tail(&body, "Read", 1);
    let steady = OffloadPolicy {
        gate: &gate,
        session_key: "sess",
        rebuild_boundary: false,
    };
    let second = offload_anthropic_request(&mut shallow, &margin_cfg(4), Some(&steady));
    assert_eq!(second.blocks_offloaded, 1, "must not revert to raw");
    assert_eq!(
        first_tool_result_text(&shallow),
        digest,
        "the same bytes must yield the same digest at any depth"
    );
}

/// `[user, assistant(Write), user(result), assistant, user]`: the Write
/// sits in an older assistant message.
fn session_with_old_write() -> Value {
    json!({"messages": [
        {"role":"user","content":[{"type":"text","text":"build it"}]},
        {"role":"assistant","content":[
            {"type":"tool_use","id":"tu_w","name":"Write",
             "input":{"file_path":"/a.rs","content":big_body()}}
        ]},
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":"tu_w","content":"ok"}
        ]},
        {"role":"assistant","content":"done"},
        {"role":"user","content":[{"type":"text","text":"thanks"}]}
    ]})
}

fn write_content(parsed: &Value) -> Value {
    parsed["messages"][1]["content"][0]["input"]["content"].clone()
}

#[test]
fn agent_and_sendmessage_inputs_stay_verbatim_on_a_rebuild_boundary() {
    let gate = OffloadGate::new(16);
    let mut parsed = session_with_old_write();
    for (i, name) in ["Agent", "SendMessage"].into_iter().enumerate() {
        parsed["messages"][1]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"tool_use","id":format!("tu_{i}"),"name":name,
                    "input":{"prompt":big_body()}}));
    }
    let out = offload_tool_use_inputs(
        &mut parsed,
        &cfg(200),
        &gate_policy(&gate, true),
        Some(5),
        &|_: &OffloadRecord| true,
    );
    assert_eq!(out.blocks_offloaded, 1, "only the Write converts");
    for i in [1, 2] {
        assert_eq!(
            parsed["messages"][1]["content"][i]["input"]["prompt"],
            Value::String(big_body())
        );
    }
}

#[test]
fn an_older_assistant_message_is_never_converted_on_a_steady_turn() {
    let gate = OffloadGate::new(16);
    let put = |_: &OffloadRecord| true;

    let mut steady = session_with_old_write();
    let out = offload_tool_use_inputs(
        &mut steady,
        &cfg(200),
        &gate_policy(&gate, false),
        Some(5),
        &put,
    );
    assert_eq!(
        out.blocks_offloaded, 0,
        "converted inside the cached prefix"
    );
    assert_eq!(out.blocks_deferred, 1);
    assert_eq!(write_content(&steady), Value::String(big_body()));
    assert_eq!(
        steady["messages"][1]["content"][0]["input"]["file_path"], "/a.rs",
        "other keys stay as sent"
    );

    let mut boundary = session_with_old_write();
    let out = offload_tool_use_inputs(
        &mut boundary,
        &cfg(200),
        &gate_policy(&gate, true),
        Some(5),
        &put,
    );
    assert_eq!(out.blocks_offloaded, 1);
    let digest = write_content(&boundary);
    assert!(digest.as_str().unwrap().contains(MARKER_PREFIX));
    assert_eq!(out.records[0].original, big_body());
    assert_eq!(out.records[0].title, "Write");

    // Once converted, a steady turn re-applies the same bytes.
    let mut again = session_with_old_write();
    let out = offload_tool_use_inputs(
        &mut again,
        &cfg(200),
        &gate_policy(&gate, false),
        Some(5),
        &put,
    );
    assert_eq!(out.blocks_offloaded, 1, "must not revert to raw");
    assert_eq!(write_content(&again), digest);
}

#[test]
fn the_newest_assistant_message_converts_only_where_it_was_never_forwarded() {
    let gate = OffloadGate::new(16);
    let put = |_: &OffloadRecord| true;
    // Three messages: the Write is the newest assistant message.
    let mut body = session_with_old_write();
    body["messages"].as_array_mut().unwrap().truncate(3);

    let mut sent = body.clone();
    let out = offload_tool_use_inputs(
        &mut sent,
        &cfg(200),
        &gate_policy(&gate, false),
        Some(2),
        &put,
    );
    assert_eq!(out.blocks_offloaded, 0, "index 1 was already forwarded");

    let mut unknown = body.clone();
    let out = offload_tool_use_inputs(
        &mut unknown,
        &cfg(200),
        &gate_policy(&gate, false),
        None,
        &put,
    );
    assert_eq!(out.blocks_offloaded, 0, "unsure means untouched");

    let mut fresh = body.clone();
    let out = offload_tool_use_inputs(
        &mut fresh,
        &cfg(200),
        &gate_policy(&gate, false),
        Some(1),
        &put,
    );
    assert_eq!(out.blocks_offloaded, 1);
}

#[test]
fn a_failed_store_write_leaves_the_tool_use_raw() {
    let gate = OffloadGate::new(16);
    let mut body = session_with_old_write();
    let out = offload_tool_use_inputs(
        &mut body,
        &cfg(200),
        &gate_policy(&gate, true),
        Some(5),
        &|_| false,
    );
    assert_eq!(out.blocks_offloaded, 0);
    assert_eq!(write_content(&body), Value::String(big_body()));
    assert!(
        !gate.contains(
            "sess",
            &format!("{TOOL_USE_GATE_NS}{}", compute_key(big_body().as_bytes()))
        ),
        "nothing to re-apply next turn"
    );
}

fn window_cfg(stale_margin: usize, stale_window: usize) -> CtxOffloadConfig {
    CtxOffloadConfig {
        min_bytes: 200,
        stale_margin,
        stale_window,
        cross_session_seed: false,
    }
}

fn gate_policy<'a>(gate: &'a OffloadGate, boundary: bool) -> OffloadPolicy<'a> {
    OffloadPolicy {
        gate,
        session_key: "sess",
        rebuild_boundary: boundary,
    }
}

/// Just past the margin the rewrite a conversion costs is small — the last
/// four messages are 1,460 tokens where a qualifying block saves 2,280 — so
/// it pays for itself in about ten turns and does not need to wait for a
/// boundary that may never come.
#[test]
fn a_block_inside_the_window_converts_without_a_boundary() {
    let body = big_body();
    let mut parsed = req_with_tail(&body, "Read", 4);
    let gate = OffloadGate::new(16);
    let out = offload_anthropic_request(
        &mut parsed,
        &window_cfg(4, 4),
        Some(&gate_policy(&gate, false)),
    );
    assert_eq!(out.blocks_offloaded, 1, "distance 4 is inside the window");
    assert_eq!(out.blocks_deferred, 0);
}

/// Deeper than the window the same trade needs hundreds of turns, because
/// everything after the block is rewritten. Those still wait for a turn that
/// is rewriting anyway.
#[test]
fn a_block_past_the_window_still_waits_for_a_boundary() {
    let body = big_body();
    let mut parsed = req_with_tail(&body, "Read", 20);
    let gate = OffloadGate::new(16);
    let out = offload_anthropic_request(
        &mut parsed,
        &window_cfg(4, 4),
        Some(&gate_policy(&gate, false)),
    );
    assert_eq!(out.blocks_offloaded, 0, "distance 20 is far too deep");
    assert_eq!(out.blocks_deferred, 1);
}

/// The reason the set is on disk at all: a restart used to turn every digest
/// back into raw bytes. Deep blocks then forwarded raw where the provider had
/// cached a digest — a mid-history prefix break — and, being too deep for the
/// window and with no boundary in sight, they stayed raw. 296 blocks sat in
/// that state after one afternoon of restarts.
#[test]
fn a_conversion_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let body = big_body();

    // First process: converts at a boundary and records the hash.
    let before = OffloadGate::with_persistence(16, dir.path().to_path_buf());
    let mut deep = req_with_tail(&body, "Read", 20);
    let first = offload_anthropic_request(
        &mut deep,
        &margin_cfg(4),
        Some(&OffloadPolicy {
            gate: &before,
            session_key: "sess",
            rebuild_boundary: true,
        }),
    );
    assert_eq!(first.blocks_offloaded, 1, "converts on the boundary turn");
    let digest = first_tool_result_text(&deep);

    // Second process, same directory, no boundary: the block is deep and
    // would be a first conversion again if the set had not survived.
    let after = OffloadGate::with_persistence(16, dir.path().to_path_buf());
    let mut same = req_with_tail(&body, "Read", 20);
    let second = offload_anthropic_request(
        &mut same,
        &margin_cfg(4),
        Some(&OffloadPolicy {
            gate: &after,
            session_key: "sess",
            rebuild_boundary: false,
        }),
    );
    assert_eq!(
        second.blocks_offloaded, 1,
        "a restart must not revert a digest to raw bytes"
    );
    assert_eq!(second.blocks_deferred, 0);
    assert_eq!(
        first_tool_result_text(&same),
        digest,
        "and the bytes must be the same digest, or the prefix broke anyway"
    );
}

/// Without a directory the old behaviour stands, so the fallback is the thing
/// that shipped for months rather than something new and untested.
#[test]
fn without_a_directory_a_restart_still_forgets() {
    let body = big_body();
    let before = OffloadGate::new(16);
    let mut deep = req_with_tail(&body, "Read", 20);
    offload_anthropic_request(
        &mut deep,
        &margin_cfg(4),
        Some(&OffloadPolicy {
            gate: &before,
            session_key: "sess",
            rebuild_boundary: true,
        }),
    );
    let after = OffloadGate::new(16);
    let mut same = req_with_tail(&body, "Read", 20);
    let out = offload_anthropic_request(
        &mut same,
        &margin_cfg(4),
        Some(&OffloadPolicy {
            gate: &after,
            session_key: "sess",
            rebuild_boundary: false,
        }),
    );
    assert_eq!(
        out.blocks_deferred, 1,
        "in-memory only: forgotten, deferred"
    );
}

/// A session key can carry a credential. It may appear in the filename only
/// as a SHA-256, and never inside the file.
#[test]
fn the_session_key_is_never_written_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let secret = "sk-ant-oat01-a-real-looking-credential";
    let gate = OffloadGate::with_persistence(16, dir.path().to_path_buf());
    gate.record(secret, "deadbeefcafe");

    let mut files = 0;
    for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
        files += 1;
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(
            !name.contains(secret),
            "the session key must not appear in a filename"
        );
        let body = std::fs::read_to_string(entry.path()).unwrap();
        assert!(
            !body.contains(secret),
            "the session key must not appear in the file"
        );
        assert!(
            body.contains("deadbeefcafe"),
            "the hash itself is the payload"
        );
    }
    assert_eq!(files, 1, "one file per session");
}

/// A set older than the window names conversions the provider has long since
/// dropped, and whose originals may be past the store's own TTL.
#[test]
fn a_stale_set_is_not_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let gate = OffloadGate::with_persistence(16, dir.path().to_path_buf());
    gate.record("sess", "abc123");

    // Age the file's contents past the window.
    let path = gate_path(dir.path(), "sess");
    let mut snapshot: PersistedGate =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    snapshot.saved_at_unix = gate_unix_now() - GATE_PERSIST_MAX_AGE.as_secs() - 1;
    std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

    let reopened = OffloadGate::with_persistence(16, dir.path().to_path_buf());
    assert!(
        !reopened.contains("sess", "abc123"),
        "a set past the window must not be honoured"
    );
}

/// Two turns of one session converting at the same moment used to race
/// the snapshot to disk through one shared temporary file. Whichever
/// finished last won, so a hash could be present in memory and absent on
/// disk; after a restart its digest reverted to raw and busted the cache.
/// Every hash recorded by any thread must be on disk once they all return.
#[test]
fn concurrent_records_all_reach_disk() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(OffloadGate::with_persistence(16, dir.path().to_path_buf()));
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                for i in 0..25 {
                    gate.record("sess", &format!("h{t}-{i}"));
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }

    let reopened = OffloadGate::with_persistence(16, dir.path().to_path_buf());
    for t in 0..8 {
        for i in 0..25 {
            assert!(
                reopened.contains("sess", &format!("h{t}-{i}")),
                "h{t}-{i} was recorded but is not on disk"
            );
        }
    }
    let leftovers = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "tmp")
        })
        .count();
    assert_eq!(leftovers, 0, "temporary files must be renamed away");
}

/// The PR-J5 guard exists to shout when frozen history converts on a quiet
/// turn, which the window now does on purpose. Counted together, the guard
/// fired twice in the first 15 turns after `stale_window` shipped — and a
/// guard that fires on correct behaviour hides the next real bug.
#[test]
fn a_window_conversion_does_not_trip_the_thrash_guard() {
    let body = big_body();
    let mut parsed = req_with_tail(&body, "Read", 4);
    let gate = OffloadGate::new(16);
    let out = offload_anthropic_request(
        &mut parsed,
        &window_cfg(4, 4),
        Some(&gate_policy(&gate, false)),
    );
    assert_eq!(out.blocks_offloaded, 1);
    assert_eq!(out.window_offloads, 1, "counted as a window conversion");
    assert_eq!(
        out.frozen_new_offloads, 0,
        "the guard must stay quiet for a conversion the window authorised"
    );
}

/// The same conversion riding a boundary instead still counts as a frozen
/// one, so the guard keeps its teeth where the window does not reach.
#[test]
fn a_boundary_conversion_beyond_the_window_still_counts_as_frozen() {
    let body = big_body();
    let mut parsed = req_with_tail(&body, "Read", 20);
    let gate = OffloadGate::new(16);
    let out = offload_anthropic_request(
        &mut parsed,
        &window_cfg(4, 4),
        Some(&gate_policy(&gate, true)),
    );
    assert_eq!(
        out.blocks_offloaded, 1,
        "a boundary lets the deep one through"
    );
    assert_eq!(out.window_offloads, 0, "too deep to be a window conversion");
    assert_eq!(out.frozen_new_offloads, 1);
}

/// A zero window is the boundary-only behaviour, which is the default.
#[test]
fn a_zero_window_waits_even_next_to_the_tail() {
    let body = big_body();
    let mut parsed = req_with_tail(&body, "Read", 4);
    let gate = OffloadGate::new(16);
    let out = offload_anthropic_request(
        &mut parsed,
        &window_cfg(4, 0),
        Some(&gate_policy(&gate, false)),
    );
    assert_eq!(out.blocks_offloaded, 0);
    assert_eq!(out.blocks_deferred, 1);
}

/// The two fields only ever add. `stale_margin` used to decide when
/// `--exclude-tools` stopped protecting a block; with that gone, the sole
/// remaining reader is the near-tail band, `distance < margin + window`.
/// A block one message from the tail is inside a 4+4 band and converts
/// without waiting for a boundary.
#[test]
fn the_near_tail_band_is_the_margin_plus_the_window() {
    let body = big_body();
    let gate = OffloadGate::new(16);

    let mut near = req_with_tail(&body, "Read", 1);
    let out = offload_anthropic_request(
        &mut near,
        &window_cfg(4, 4),
        Some(&gate_policy(&gate, false)),
    );
    assert_eq!(out.blocks_offloaded, 1, "distance 1 is inside a band of 8");

    // Outside the band, the boundary wait still applies. A fresh gate,
    // because the block above has the same bytes and so the same hash —
    // reusing the gate would make this one a re-application, which passes
    // whatever its distance.
    let far_gate = OffloadGate::new(16);
    let mut far = req_with_tail(&body, "Read", 20);
    let out = offload_anthropic_request(
        &mut far,
        &window_cfg(4, 4),
        Some(&gate_policy(&far_gate, false)),
    );
    assert_eq!(
        out.blocks_offloaded, 0,
        "distance 20 is outside a band of 8"
    );
    assert_eq!(first_tool_result_text(&far), body, "content untouched");
}

/// I1 restated for this field: position decides *whether* a block converts,
/// never *what* it converts to. If the digest moved with depth, every turn
/// would rewrite the prefix.
#[test]
fn the_digest_does_not_depend_on_position() {
    let body = big_body();
    let mut near = req_with_tail(&body, "Bash", 4);
    let mut far = req_with_tail(&body, "Bash", 40);
    offload_anthropic_request(&mut near, &cfg(200), None);
    offload_anthropic_request(&mut far, &cfg(200), None);
    assert_eq!(first_tool_result_text(&near), first_tool_result_text(&far));
}

/// The preview has to scale, or `min_bytes` below ~3.2KB is a no-op: a fixed
/// 3,072-byte cut cannot shrink a 4,000-byte block.
/// Prose no compressor claims, so the digest takes the preview fallback.
fn prose_body() -> String {
    (0..120)
        .map(|i| {
            format!(
                "Paragraph {i}: the quick brown fox jumps over the lazy dog {}.\n",
                i * 7
            )
        })
        .collect()
}

/// The preview text of the first tool_result's digest, without its footer.
fn preview_of(parsed: &Value) -> String {
    let text = first_tool_result_text(parsed);
    assert!(
        text.contains("…[truncated"),
        "digest took the preview fallback"
    );
    text[..text.find("\n…[truncated").unwrap()].to_string()
}

fn frozen_req(body: &str) -> Value {
    req_with_tail(body, "Bash", 12)
}

#[test]
fn a_block_converted_under_the_legacy_entry_keeps_its_legacy_preview() {
    let body = prose_body();
    let hash = compute_key(body.as_bytes());
    let gate = OffloadGate::new(4);
    gate.record("s", &hash);
    let policy = OffloadPolicy {
        gate: &gate,
        session_key: "s",
        rebuild_boundary: false,
    };
    let mut parsed = frozen_req(&body);
    let out = offload_anthropic_request(&mut parsed, &margin_cfg(4), Some(&policy));
    assert_eq!(
        out.blocks_offloaded, 1,
        "a prior conversion re-applies off a boundary"
    );
    let preview = preview_of(&parsed);
    assert_eq!(preview.len(), preview_budget(body.len()));
    assert!(preview.len() > PREVIEW_CAP_BYTES);
    assert!(!gate.contains("s", &format!("{PREVIEW_CAP_GATE_NS}{hash}")));
}

#[test]
fn a_new_block_takes_the_capped_preview_and_keeps_it() {
    let body = prose_body();
    let hash = compute_key(body.as_bytes());
    let gate = OffloadGate::new(4);
    let policy = OffloadPolicy {
        gate: &gate,
        session_key: "s",
        rebuild_boundary: true,
    };
    let mut first = frozen_req(&body);
    offload_anthropic_request(&mut first, &margin_cfg(4), Some(&policy));
    assert_eq!(preview_of(&first).len(), PREVIEW_CAP_BYTES);
    assert!(gate.contains("s", &format!("{PREVIEW_CAP_GATE_NS}{hash}")));
    assert!(
        !gate.contains("s", &hash),
        "no legacy entry for a capped conversion"
    );

    let steady = OffloadPolicy {
        gate: &gate,
        session_key: "s",
        rebuild_boundary: false,
    };
    let mut second = frozen_req(&body);
    offload_anthropic_request(&mut second, &margin_cfg(4), Some(&steady));
    assert_eq!(
        first_tool_result_text(&first),
        first_tool_result_text(&second)
    );
}

#[test]
fn a_legacy_entry_read_from_disk_keeps_its_legacy_preview() {
    let body = prose_body();
    let hash = compute_key(body.as_bytes());
    let dir = tempfile::tempdir().unwrap();
    // A gate file written before the cap existed: un-namespaced hashes only.
    let file = PersistedGate {
        saved_at_unix: gate_unix_now(),
        hashes: vec![hash.clone()],
    };
    std::fs::write(
        gate_path(dir.path(), "s"),
        serde_json::to_vec(&file).unwrap(),
    )
    .unwrap();

    let gate = OffloadGate::with_persistence(4, dir.path().to_path_buf());
    let policy = OffloadPolicy {
        gate: &gate,
        session_key: "s",
        rebuild_boundary: false,
    };
    let mut parsed = frozen_req(&body);
    let out = offload_anthropic_request(&mut parsed, &margin_cfg(4), Some(&policy));
    assert_eq!(out.blocks_offloaded, 1);
    assert_eq!(preview_of(&parsed).len(), preview_budget(body.len()));
}

#[test]
fn the_preview_budget_scales_with_the_block() {
    assert_eq!(preview_budget(100), PREVIEW_FLOOR_BYTES, "floor holds");
    assert_eq!(preview_budget(8_000), 2_000, "a quarter in the middle");
    assert_eq!(preview_budget(10_000_000), PREVIEW_BYTES, "ceiling holds");
    for len in [4_000usize, 20_000, 200_000] {
        assert!(
            preview_budget(len) < len,
            "a {len}-byte block must be able to shrink"
        );
    }
}

#[test]
fn offloads_oversized_block_with_marker() {
    // Repetitive log content compresses well and is comfortably > 200 B.
    let body = "ERROR: disk full\n".repeat(50);
    let mut parsed = req(&body);
    let out = offload_anthropic_request(&mut parsed, &cfg(200), None);
    assert_eq!(out.blocks_offloaded, 1);
    assert_eq!(out.records.len(), 1);
    let text = first_tool_result_text(&parsed);
    assert!(text.contains("<<ctx:"));
    assert!(text.contains("bytes offloaded"));
    // Title is the paired bash command (deterministic).
    assert_eq!(out.records[0].title, "cat big.log");
    assert_eq!(out.records[0].original, body);
    assert_eq!(out.records[0].hash.len(), 24);
}

#[test]
fn plaintext_read_output_offloads_via_preview_fallback() {
    // Simulates a Claude Code `Read` result: line-number prefixes make the
    // content classify as PlainText, where no structural compressor
    // applies (kompress is off by default). The preview fallback must
    // still offload it — this was silently a no-op before.
    let mut body = String::new();
    for i in 1..=1900 {
        body.push_str(&format!(
            "{i}\t[[package]]\nname = \"crate-{i}\"\nchecksum = \"abc{i}\"\n"
        ));
    }
    assert!(body.len() > 50_000);
    let mut parsed = req(&body);
    let out = offload_anthropic_request(&mut parsed, &cfg(50_000), None);
    assert_eq!(out.blocks_offloaded, 1);
    let text = first_tool_result_text(&parsed);
    assert!(
        text.len() < 4096,
        "digest must be small, got {}",
        text.len()
    );
    assert!(text.starts_with("1\t[[package]]"), "preview keeps the head");
    assert!(text.contains("truncated"));
    assert!(text.contains(&format!(
        "use the headroom_retrieve tool with hash=\"{}\"",
        out.records[0].hash
    )));
    assert_eq!(out.records[0].original, body);
}

#[test]
fn preview_cuts_on_char_boundary() {
    // A multi-byte char straddling the budget must not split.
    let body = format!("{}é{}", "x".repeat(3071), "y".repeat(60_000));
    let cut = preview(&body, PREVIEW_BYTES);
    assert!(cut.starts_with(&"x".repeat(3071)));
    assert!(!cut.contains('\u{FFFD}'));
    assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
}

#[test]
fn below_threshold_is_untouched() {
    let body = "small output";
    let mut parsed = req(body);
    let out = offload_anthropic_request(&mut parsed, &cfg(50_000), None);
    assert_eq!(out.blocks_offloaded, 0);
    assert_eq!(first_tool_result_text(&parsed), body);
}

#[test]
fn digest_is_deterministic_same_bytes_twice() {
    let body = "ERROR: disk full\n".repeat(50);
    let mut a = req(&body);
    let mut b = req(&body);
    offload_anthropic_request(&mut a, &cfg(200), None);
    offload_anthropic_request(&mut b, &cfg(200), None);
    assert_eq!(a, b, "same input bytes must produce identical output bytes");
}

#[test]
fn already_offloaded_block_passes_through() {
    let body = "ERROR: disk full\n".repeat(50);
    let mut parsed = req(&body);
    offload_anthropic_request(&mut parsed, &cfg(200), None);
    let after_first = parsed.clone();
    // Re-running must be a no-op (idempotent): the digest already carries
    // the marker.
    let out = offload_anthropic_request(&mut parsed, &cfg(200), None);
    assert_eq!(out.blocks_offloaded, 0);
    assert_eq!(parsed, after_first);
}

/// The pointer has to name a tool the model can actually call, and say
/// in words that it is a tool. Pinned because both halves have been got
/// wrong: first a shell command the model cannot run, then a bare call
/// signature that sessions mistook for a skill.
#[test]
fn marker_format_is_pinned() {
    let f = footer("abc123", 1234);
    assert_eq!(
        f,
        "\n<<ctx:abc123>> (1234 bytes offloaded; \
             use the headroom_retrieve tool with hash=\"abc123\")"
    );
}

#[test]
fn array_content_shape_preserved_as_text_block() {
    let body = "ERROR: disk full\n".repeat(50);
    let mut parsed = json!({
        "model": "claude-3-5-sonnet-20241022",
        "messages": [
            {"role":"assistant","content":[
                {"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"x"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu_1",
                 "content":[{"type":"text","text": body}]}
            ]}
        ]
    });
    let out = offload_anthropic_request(&mut parsed, &cfg(200), None);
    assert_eq!(out.blocks_offloaded, 1);
    let content = &parsed["messages"][1]["content"][0]["content"];
    assert!(content.is_array());
    assert_eq!(content[0]["type"], "text");
    assert!(content[0]["text"].as_str().unwrap().contains("<<ctx:"));
}

// ── PR-J4: boundary gate ──

/// Request with a large tool_result in FROZEN history (not the last
/// message) plus a trailing small live message.
fn req_frozen(body: &str) -> Value {
    json!({
        "model": "claude-3-5-sonnet-20241022",
        "messages": [
            {"role":"assistant","content":[
                {"type":"tool_use","id":"tu_1","name":"Bash",
                 "input":{"command":"cat big.log"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu_1","content": body}
            ]},
            {"role":"assistant","content":[{"type":"text","text":"ok"}]},
            {"role":"user","content":[{"type":"text","text":"next question"}]}
        ]
    })
}

#[test]
fn gate_defers_frozen_block_on_steady_state_turn() {
    let gate = OffloadGate::new(8);
    let body = "ERROR: disk full\n".repeat(50);
    let mut parsed = req_frozen(&body);
    let policy = OffloadPolicy {
        gate: &gate,
        session_key: "sess",
        rebuild_boundary: false,
    };
    let out = offload_anthropic_request(&mut parsed, &cfg(200), Some(&policy));
    assert_eq!(out.blocks_offloaded, 0);
    assert_eq!(out.blocks_deferred, 1);
    assert_eq!(out.frozen_new_offloads, 0);
    // Bytes untouched — the cached prefix is preserved (I4).
    assert_eq!(parsed["messages"][1]["content"][0]["content"], json!(body));
}

#[test]
fn gate_permits_frozen_block_on_rebuild_boundary_then_stays_offloaded() {
    let gate = OffloadGate::new(8);
    let body = "ERROR: disk full\n".repeat(50);

    // Boundary turn: frozen block converts (riding the client's rebuild).
    let mut parsed = req_frozen(&body);
    let boundary = OffloadPolicy {
        gate: &gate,
        session_key: "sess",
        rebuild_boundary: true,
    };
    let out = offload_anthropic_request(&mut parsed, &cfg(200), Some(&boundary));
    assert_eq!(out.blocks_offloaded, 1);
    assert_eq!(out.frozen_new_offloads, 1);

    // Next steady-state turn: client resends raw history; the monotonic
    // set re-applies the identical offload (I3 — no flip-flop).
    let mut resent = req_frozen(&body);
    let steady = OffloadPolicy {
        gate: &gate,
        session_key: "sess",
        rebuild_boundary: false,
    };
    let out2 = offload_anthropic_request(&mut resent, &cfg(200), Some(&steady));
    assert_eq!(out2.blocks_offloaded, 1);
    assert_eq!(out2.blocks_deferred, 0);
    assert_eq!(
        out2.frozen_new_offloads, 0,
        "re-application is not a new frozen conversion"
    );
    assert_eq!(
        parsed, resent,
        "boundary turn and re-application produce identical bytes"
    );
}

#[test]
fn gate_permits_live_tail_block_without_boundary() {
    let gate = OffloadGate::new(8);
    let body = "ERROR: disk full\n".repeat(50);
    // `req` puts the tool_result in the LAST message (live tail).
    let mut parsed = req(&body);
    let policy = OffloadPolicy {
        gate: &gate,
        session_key: "sess",
        rebuild_boundary: false,
    };
    let out = offload_anthropic_request(&mut parsed, &cfg(200), Some(&policy));
    assert_eq!(out.blocks_offloaded, 1);
    assert_eq!(out.frozen_new_offloads, 0);
    assert!(first_tool_result_text(&parsed).contains("<<ctx:"));
}

#[test]
fn gate_offload_set_is_session_scoped() {
    let gate = OffloadGate::new(8);
    let body = "ERROR: disk full\n".repeat(50);

    // Session A converts on a boundary.
    let mut a = req_frozen(&body);
    let pa = OffloadPolicy {
        gate: &gate,
        session_key: "sess-A",
        rebuild_boundary: true,
    };
    assert_eq!(
        offload_anthropic_request(&mut a, &cfg(200), Some(&pa)).blocks_offloaded,
        1
    );

    // Session B on a steady-state turn must NOT inherit A's set.
    let mut b = req_frozen(&body);
    let pb = OffloadPolicy {
        gate: &gate,
        session_key: "sess-B",
        rebuild_boundary: false,
    };
    let out = offload_anthropic_request(&mut b, &cfg(200), Some(&pb));
    assert_eq!(out.blocks_offloaded, 0);
    assert_eq!(out.blocks_deferred, 1);
}

// ─── Cross-session gate seeding (P1 proof) ────────────────────────
//
// A model switch mints a fresh session key, so the new lineage's gate is
// empty and its frozen history would stall Deferred until a boundary.
// Seeding the newborn session from the donor's converted set lets it
// convert known blocks on first sight, through the existing `prior`
// path, with byte-identical digests.

fn frozen_result_text(parsed: &Value) -> String {
    tool_result_text(&parsed["messages"][1]["content"][0]["content"]).unwrap_or_default()
}

/// Donor converts on a boundary; returns the gate and the digest bytes.
fn donor_with_one_conversion(gate: &OffloadGate, body: &str) -> String {
    let mut donor = req_frozen(body);
    let policy = OffloadPolicy {
        gate,
        session_key: "sess-opus",
        rebuild_boundary: true,
    };
    let out = offload_anthropic_request(&mut donor, &cfg(200), Some(&policy));
    assert_eq!(out.blocks_offloaded, 1, "donor converts on the boundary");
    frozen_result_text(&donor)
}

#[test]
fn seeded_newborn_converts_on_first_sight_without_boundary() {
    let gate = OffloadGate::new(8);
    let body = "ERROR: disk full\n".repeat(50);
    let donor_digest = donor_with_one_conversion(&gate, &body);

    let outcome = gate.seed_if_absent("sess-opus", "sess-sonnet");
    assert_eq!(outcome, SeedOutcome::Seeded { count: 1 });

    // Same frozen history, steady-state turn, NEW session: converts on
    // first sight — the Deferred stall is gone.
    let mut fresh = req_frozen(&body);
    let policy = OffloadPolicy {
        gate: &gate,
        session_key: "sess-sonnet",
        rebuild_boundary: false,
    };
    let out = offload_anthropic_request(&mut fresh, &cfg(200), Some(&policy));
    assert_eq!(
        out.blocks_offloaded, 1,
        "seeded prior rides, no boundary needed"
    );
    assert_eq!(out.blocks_deferred, 0);
    assert_eq!(
        frozen_result_text(&fresh),
        donor_digest,
        "seeded digest is byte-identical to the donor's"
    );
}

#[test]
fn seeded_session_reapplies_identically_on_later_turns() {
    // I2/cache-safety: what the seeded session emits on turn 1 it must
    // re-emit on turn 2, or the upstream prefix diverges and re-caches.
    let gate = OffloadGate::new(8);
    let body = "ERROR: disk full\n".repeat(50);
    donor_with_one_conversion(&gate, &body);
    assert_eq!(
        gate.seed_if_absent("sess-opus", "sess-sonnet"),
        SeedOutcome::Seeded { count: 1 }
    );

    let turn1 = {
        let mut parsed = req_frozen(&body);
        let policy = OffloadPolicy {
            gate: &gate,
            session_key: "sess-sonnet",
            rebuild_boundary: false,
        };
        offload_anthropic_request(&mut parsed, &cfg(200), Some(&policy));
        frozen_result_text(&parsed)
    };
    let turn2 = {
        let mut parsed = req_frozen(&body);
        let policy = OffloadPolicy {
            gate: &gate,
            session_key: "sess-sonnet",
            rebuild_boundary: false,
        };
        let out = offload_anthropic_request(&mut parsed, &cfg(200), Some(&policy));
        assert_eq!(
            out.blocks_offloaded, 1,
            "still converted, never reverts to raw"
        );
        frozen_result_text(&parsed)
    };
    assert_eq!(turn1, turn2, "byte-stable across turns");
}

#[test]
fn seed_into_live_session_is_refused_and_changes_nothing() {
    let gate = OffloadGate::new(8);
    let donor_body = "ERROR: disk full\n".repeat(50);
    donor_with_one_conversion(&gate, &donor_body);

    // Session C lives its own life first: converts a DIFFERENT block.
    let own_body = "OTHER: out of inodes\n".repeat(50);
    let mut c = req_frozen(&own_body);
    let pc = OffloadPolicy {
        gate: &gate,
        session_key: "sess-C",
        rebuild_boundary: true,
    };
    assert_eq!(
        offload_anthropic_request(&mut c, &cfg(200), Some(&pc)).blocks_offloaded,
        1
    );

    assert_eq!(
        gate.seed_if_absent("sess-opus", "sess-C"),
        SeedOutcome::RefusedLive
    );
    // C's set holds only its own conversion — donor hashes NOT merged.
    // Merging now would flip C's previously-raw donor-shaped blocks
    // mid-history and bust its prefix.
    let sessions = gate.sessions.lock().unwrap();
    let c_set = sessions.peek("sess-C").expect("C recorded");
    assert_eq!(c_set.len(), 1);
    let donor_set = sessions.peek("sess-opus").expect("donor recorded");
    assert!(
        c_set.is_disjoint(donor_set),
        "live session keeps exactly its own conversions"
    );
}

#[test]
fn hydrate_installed_empty_set_counts_as_live() {
    // S1a pin: absence alone is not the birth signal. An empty set —
    // installed by `hydrate` on miss, or held by a live session that
    // simply never converted — must refuse seeding, or turn-11 seeding
    // of a 10-turn-raw session would shift its prefix. The caller gates
    // on drift first-sight; this refusal is the backstop.
    let gate = OffloadGate::new(8);
    let body = "ERROR: disk full\n".repeat(50);
    donor_with_one_conversion(&gate, &body);

    gate.sessions
        .lock()
        .unwrap()
        .put("sess-live-empty".to_string(), HashSet::new());
    assert_eq!(
        gate.seed_if_absent("sess-opus", "sess-live-empty"),
        SeedOutcome::RefusedLive
    );
}

#[test]
fn seed_if_absent_trusts_the_caller_birth_signal() {
    // The callee refuses KNOWN sessions; it cannot tell a newborn from a
    // live-never-converted session (both hold no set). Birth knowledge
    // lives with the caller (drift first-sight) — pin that contract here
    // so a future "improvement" to this method cannot silently assume it.
    let gate = OffloadGate::new(8);
    let body = "ERROR: disk full\n".repeat(50);
    donor_with_one_conversion(&gate, &body);

    // Live session, three steady-state turns, nothing converted, no set.
    for _ in 0..3 {
        let mut parsed = req_frozen(&body);
        let policy = OffloadPolicy {
            gate: &gate,
            session_key: "sess-live-raw",
            rebuild_boundary: false,
        };
        let out = offload_anthropic_request(&mut parsed, &cfg(200), Some(&policy));
        assert_eq!(out.blocks_offloaded, 0);
    }
    // Callee proceeds (absent); only the caller's birth gate makes this
    // safe. If this assertion ever flips to RefusedLive, the caller
    // contract changed — update S1a accordingly.
    assert_eq!(
        gate.seed_if_absent("sess-opus", "sess-live-raw"),
        SeedOutcome::Seeded { count: 1 }
    );
}

#[test]
fn donor_is_untouched_by_seeding() {
    let gate = OffloadGate::new(8);
    let body = "ERROR: disk full\n".repeat(50);
    donor_with_one_conversion(&gate, &body);
    let before = gate
        .sessions
        .lock()
        .unwrap()
        .peek("sess-opus")
        .cloned()
        .expect("donor recorded");
    assert_eq!(
        gate.seed_if_absent("sess-opus", "sess-sonnet"),
        SeedOutcome::Seeded { count: 1 }
    );
    let after = gate
        .sessions
        .lock()
        .unwrap()
        .peek("sess-opus")
        .cloned()
        .expect("donor still recorded");
    assert_eq!(before, after, "donor lineage bit-for-bit unaffected");
}

#[test]
fn empty_donor_installs_nothing() {
    let gate = OffloadGate::new(8);
    assert_eq!(
        gate.seed_if_absent("sess-nobody", "sess-new"),
        SeedOutcome::DonorEmpty
    );
    assert!(
        !gate.sessions.lock().unwrap().contains("sess-new"),
        "no empty set installed"
    );
}

#[test]
fn seed_truncates_pathological_donor_sets() {
    let gate = OffloadGate::new(8);
    for i in 0..(SEED_MAX_HASHES + 10) {
        gate.record("sess-big", &format!("hash{i:06}"));
    }
    assert_eq!(
        gate.seed_if_absent("sess-big", "sess-new"),
        SeedOutcome::Seeded {
            count: SEED_MAX_HASHES
        }
    );
    assert_eq!(
        gate.sessions
            .lock()
            .unwrap()
            .peek("sess-new")
            .unwrap()
            .len(),
        SEED_MAX_HASHES
    );
}

#[test]
fn seed_newborn_session_wires_lineage_to_first_sight_conversion() {
    // End-to-end at the helper level, exactly as the Claude/routed drift
    // first-sight paths call it: donor birth recorded, newborn seeded
    // from the same lineage under a different model, steady-state turn
    // converts on first sight.
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use crate::cache_stabilization::drift_detector::{ApiKind, model_free_lineage_key};

    let gate = OffloadGate::new(8);
    let body = "ERROR: disk full\n".repeat(50);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer shared-workspace-token".parse().unwrap(),
    );
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);

    // Donor lineage converts on a boundary under its own session.
    let opus = req_frozen(&body);
    let mut donor = opus.clone();
    let donor_policy = OffloadPolicy {
        gate: &gate,
        session_key: "sess-opus",
        rebuild_boundary: true,
    };
    assert_eq!(
        offload_anthropic_request(&mut donor, &cfg(200), Some(&donor_policy)).blocks_offloaded,
        1
    );
    let donor_digest = frozen_result_text(&donor);

    // Newborn lineage: different model, same opener + credential.
    let mut sonnet = opus.clone();
    sonnet["model"] = serde_json::json!("sonnet-small");

    // What the drift first-sight path does for the donor at its birth,
    // then what it does for the newborn: record + seed attempt.
    let donor_lineage =
        model_free_lineage_key(&headers, &addr, &opus, ApiKind::Anthropic).expect("lineage");
    gate.note_session_birth(&donor_lineage, "sess-opus");
    seed_newborn_session(
        &gate,
        &headers,
        &addr,
        &sonnet,
        ApiKind::Anthropic,
        "sess-sonnet",
        "req-1",
    );

    // Steady-state turn on the new session converts on first sight with
    // the donor's exact bytes.
    let mut fresh = sonnet.clone();
    let policy = OffloadPolicy {
        gate: &gate,
        session_key: "sess-sonnet",
        rebuild_boundary: false,
    };
    let out = offload_anthropic_request(&mut fresh, &cfg(200), Some(&policy));
    assert_eq!(out.blocks_offloaded, 1, "seeded lineage converts on sight");
    assert_eq!(out.blocks_deferred, 0);
    assert_eq!(frozen_result_text(&fresh), donor_digest);
}

#[test]
fn lineage_birth_order_and_candidates() {
    let gate = OffloadGate::new(8);
    let lineage = ("auth:abc".to_string(), "msghash".to_string());
    assert!(gate.seed_candidates(&lineage, "sess-x").is_empty());
    gate.note_session_birth(&lineage, "sess-A");
    gate.note_session_birth(&lineage, "sess-B");
    gate.note_session_birth(&lineage, "sess-A"); // re-birth dedups, refreshes
    assert_eq!(
        gate.seed_candidates(&lineage, "sess-B"),
        vec!["sess-A".to_string()]
    );
    assert_eq!(
        gate.seed_candidates(&lineage, "sess-A"),
        vec!["sess-B".to_string()]
    );
    // Bounded per lineage.
    for i in 0..(SEED_LINEAGE_CAP + 3) {
        gate.note_session_birth(&lineage, &format!("sess-{i}"));
    }
    assert_eq!(
        gate.seed_candidates(&lineage, "nobody").len(),
        SEED_LINEAGE_CAP
    );
}
