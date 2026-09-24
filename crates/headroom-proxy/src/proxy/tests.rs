use super::*;

#[tokio::test]
async fn provider_client_routes_through_socks5h_proxy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let socks_server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        let mut greeting = [0; 3];
        stream.read_exact(&mut greeting).await.unwrap();
        assert_eq!(
            greeting,
            [5, 1, 2],
            "client requests SOCKS5 username/password auth"
        );
        stream.write_all(&[5, 2]).await.unwrap();
        let mut auth_header = [0; 2];
        stream.read_exact(&mut auth_header).await.unwrap();
        assert_eq!(auth_header[0], 1, "RFC 1929 auth version");
        let mut username = vec![0; usize::from(auth_header[1])];
        stream.read_exact(&mut username).await.unwrap();
        let mut password_len = [0; 1];
        stream.read_exact(&mut password_len).await.unwrap();
        let mut password = vec![0; usize::from(password_len[0])];
        stream.read_exact(&mut password).await.unwrap();
        assert_eq!(username, b"testuser");
        assert_eq!(password, b"testpass");
        stream.write_all(&[1, 0]).await.unwrap();

        let mut request_header = [0; 4];
        stream.read_exact(&mut request_header).await.unwrap();
        assert_eq!(request_header, [5, 1, 0, 3], "domain-name CONNECT");
        let mut host_len = [0; 1];
        stream.read_exact(&mut host_len).await.unwrap();
        let mut host = vec![0; usize::from(host_len[0])];
        stream.read_exact(&mut host).await.unwrap();
        let mut port = [0; 2];
        stream.read_exact(&mut port).await.unwrap();
        assert_eq!(host, b"muse-zen.internal");
        assert_eq!(u16::from_be_bytes(port), 8087);

        // Accept the tunnel and act as the target HTTP server. No DNS or
        // external network access is needed for this end-to-end check.
        stream
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
            .await
            .unwrap();
        let mut request = [0; 1024];
        let n = stream.read(&mut request).await.unwrap();
        assert!(request[..n].starts_with(b"POST /v1/messages HTTP/1.1"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
            .await
            .unwrap();
    });

    let mut config =
        crate::config::Config::for_test("https://api.anthropic.com".parse().expect("upstream URL"));
    config.http_proxy = Some(format!("socks5h://testuser:testpass@{proxy_addr}"));
    let client = AppState::build_upstream_client(&config).expect("SOCKS5 client");
    let response = client
        .post("http://muse-zen.internal:8087/v1/messages")
        .body("test")
        .send()
        .await
        .expect("request should traverse the local SOCKS5 proxy");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "ok");
    socks_server.await.unwrap();
}

/// Simulate ten parallel Claude sibling lanes over ten independent SOCKS
/// egresses. Each first-seen lane must use a different endpoint, and a
/// later turn for the same lane must remain pinned to its original one.
/// The per-egress count behind `/debug/inflight`'s `egress_in_flight`: a
/// rotating egress refuses without leaving a count behind, the other keeps
/// serving, and a guard moved into a response body lasts until that body
/// ends or is dropped.
#[tokio::test]
async fn egress_guard_follows_the_response_body_and_respects_maintenance() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("x-probe", "kept")
                .set_body_string("hello"),
        )
        .mount(&server)
        .await;
    let pool = Arc::new(ProviderEgressPool::new(
        vec![reqwest::Client::new(), reqwest::Client::new()],
        vec!["proxy-a".to_string(), "proxy-b".to_string()],
    ));
    let count = |id: &str| pool.in_flight_by_egress()[id].as_u64().unwrap();

    let guard = pool.acquire(0).expect("idle egress");
    assert_eq!((count("proxy-a"), count("proxy-b")), (1, 0));
    assert!(pool.set_maintenance("proxy-a", true));
    assert_eq!(pool.acquire(0).err().as_deref(), Some("proxy-a"));
    assert_eq!(count("proxy-a"), 1, "a refused acquire leaves no count");
    let other = pool.acquire(1).expect("the other egress keeps serving");
    assert_eq!(count("proxy-b"), 1);
    drop(other);
    assert_eq!(count("proxy-b"), 0);

    let resp = reqwest::get(server.uri()).await.unwrap();
    let url = resp.url().clone();
    let resp = attach_egress_guard(resp, Some(guard));
    assert_eq!(count("proxy-a"), 1, "the body carries the guard");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-probe"], "kept");
    assert_eq!(resp.url(), &url);
    assert_eq!(resp.text().await.unwrap(), "hello");
    assert_eq!(count("proxy-a"), 0, "released at the end of the body");

    assert!(pool.set_maintenance("proxy-a", false));
    let guard = pool.acquire(0).expect("rotation finished");
    let resp = attach_egress_guard(reqwest::get(server.uri()).await.unwrap(), Some(guard));
    assert_eq!(count("proxy-a"), 1);
    drop(resp);
    assert_eq!(count("proxy-a"), 0, "released when the body is dropped");
}

#[tokio::test]
async fn ten_concurrent_stream_lanes_use_distinct_sticky_socks_egresses() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const LANES: usize = 10;
    let mut socks_listeners = Vec::with_capacity(LANES);
    let mut proxy_urls = Vec::with_capacity(LANES);
    for _ in 0..LANES {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        proxy_urls.push(format!("socks5h://{}", listener.local_addr().unwrap()));
        socks_listeners.push(listener);
    }

    let socks_servers: Vec<_> = socks_listeners
        .into_iter()
        .enumerate()
        .map(|(index, listener)| {
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut greeting = [0; 2];
                stream.read_exact(&mut greeting).await.unwrap();
                assert_eq!(greeting, [5, 1]);
                let mut method = [0; 1];
                stream.read_exact(&mut method).await.unwrap();
                assert_eq!(method, [0]);
                stream.write_all(&[5, 0]).await.unwrap();

                let mut request_header = [0; 4];
                stream.read_exact(&mut request_header).await.unwrap();
                assert_eq!(request_header, [5, 1, 0, 3]);
                let mut host_len = [0; 1];
                stream.read_exact(&mut host_len).await.unwrap();
                let mut host = vec![0; usize::from(host_len[0])];
                stream.read_exact(&mut host).await.unwrap();
                let mut port = [0; 2];
                stream.read_exact(&mut port).await.unwrap();
                assert_eq!(host, b"muse-zen.internal");
                assert_eq!(u16::from_be_bytes(port), 8087);
                stream
                    .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                    .await
                    .unwrap();

                let mut request = [0; 1024];
                let n = stream.read(&mut request).await.unwrap();
                assert!(request[..n].starts_with(b"POST /v1/messages HTTP/1.1"));
                let body = format!("lane-{index}");
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).await.unwrap();
            })
        })
        .collect();

    let mut config =
        crate::config::Config::for_test("https://api.anthropic.com".parse().expect("upstream URL"));
    config.zen_http_proxy_pool = proxy_urls;
    let pool = AppState::build_zen_egresses(&config)
        .expect("valid test SOCKS URLs")
        .expect("pool configured");
    let mut state = crate::test_support::test_state(|_| {});
    state.zen_egresses = Some(Arc::new(pool));

    let mut requests = Vec::with_capacity(LANES);
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer test-spark-token".parse().unwrap(),
    );
    let client_addr = "127.0.0.1:4242".parse::<SocketAddr>().unwrap();
    let mut shared_session_key = None;
    for index in 0..LANES {
        // Model ten sibling subagents with a shared credential and
        // opener but distinct agent system prompts, matching the stream
        // lane derivation used by real routed Claude requests.
        let body = serde_json::json!({
            "model": "claude-muse-spark-1.3",
            "system": format!("Spark subagent instructions {index}"),
            "messages": [{"role": "user", "content": "shared parent task opener"}]
        });
        let session_key = derive_session_key(&headers, &client_addr, &body, ApiKind::Anthropic);
        if let Some(shared) = shared_session_key.as_deref() {
            assert_eq!(
                shared, session_key,
                "siblings should share session identity"
            );
        } else {
            shared_session_key = Some(session_key.clone());
        }
        let lane_key = stream_lane_key(
            &session_key,
            &compute_structural_hash(&body, ApiKind::Anthropic),
        );
        let (client, slot, egress_id, _guard) = state
            .zen_client_for_lane(Some(&lane_key))
            .expect("new test egresses are not in maintenance");
        assert_eq!(slot, index, "first fan-out should spread one per egress");
        assert!(egress_id.starts_with("proxy-"));
        if index == 0 {
            assert!(state.set_zen_egress_maintenance(egress_id, true));
            assert!(matches!(
                state.zen_client_for_lane(Some(&lane_key)),
                Err(blocked_id) if blocked_id == egress_id
            ));
            assert!(state.set_zen_egress_maintenance(egress_id, false));
        }
        let (_, sticky_slot, sticky_id, _) = state
            .zen_client_for_lane(Some(&lane_key))
            .expect("new test egresses are not in maintenance");
        assert_eq!(sticky_slot, slot, "later turns must stay pinned");
        assert_eq!(sticky_id, egress_id);

        let client = client.clone();
        requests.push(tokio::spawn(async move {
            let response = client
                .post("http://muse-zen.internal:8087/v1/messages")
                .body("{}")
                .send()
                .await
                .expect("request traverses its assigned SOCKS egress");
            response.text().await.unwrap()
        }));
    }

    for (index, request) in requests.into_iter().enumerate() {
        assert_eq!(request.await.unwrap(), format!("lane-{index}"));
    }
    for server in socks_servers {
        server.await.unwrap();
    }
}

/// The rotation-drain contract (`GET /debug/inflight`): guards held for
/// whole turns must move the process counter up on entry and back down
/// on drop. Exact asserts are safe here — no lib test drives
/// `forward_http`/`handle_messages`, so nothing else holds a guard.
#[test]
fn inflight_count_tracks_whole_turn_guards() {
    let before = InflightGuard::count_global();
    let g1 = InflightGuard::enter();
    assert_eq!(g1.count(), before + 1);
    let g2 = InflightGuard::enter();
    assert_eq!(InflightGuard::count_global(), before + 2);
    drop(g1);
    assert_eq!(InflightGuard::count_global(), before + 1);
    drop(g2);
    assert_eq!(InflightGuard::count_global(), before);
}

/// The gap this closed: a routed turn reaches its upstream through
/// `handlers::local_model`, never through `forward_http`, so for as
/// long as the holds lived inline in `forward_http` a routed turn was
/// forwarded with its volatile `system` lines intact. Both callers go
/// through `apply_system_holds` now, and this pins that it holds.
#[test]
fn the_role_sentence_is_held_for_any_caller_not_just_forward_http() {
    const PLAIN: &str =
        "You are an interactive agent that helps users with software engineering tasks.";
    const STYLED: &str = "You are an interactive agent that helps users according to your \
             \"Output Style\", which describes how you should respond to user queries.";
    let state = crate::test_support::test_state(|c| {
        c.prefix_replay = true;
        c.hold_role_sentence = true;
    });

    let mut opening = serde_json::json!({"system": PLAIN, "messages": []});
    apply_system_holds(&state, &mut opening, "sess-1", "req-1");

    let mut flipped = serde_json::json!({"system": STYLED, "messages": []});
    apply_system_holds(&state, &mut flipped, "sess-1", "req-2");
    assert_eq!(
        flipped["system"], PLAIN,
        "the flipped sentence should have been held to the opening form"
    );
}

/// Both holds depend on `--prefix-replay`, so with replay off the body
/// must go out exactly as it came in.
#[test]
fn holds_do_nothing_without_prefix_replay() {
    const STYLED: &str = "You are an interactive agent that helps users according to your \
             \"Output Style\", which describes how you should respond to user queries.";
    let state = crate::test_support::test_state(|c| {
        c.prefix_replay = false;
        c.hold_role_sentence = true;
    });

    let mut opening = serde_json::json!({"system": "You are an interactive agent that helps \
             users with software engineering tasks.", "messages": []});
    apply_system_holds(&state, &mut opening, "sess-1", "req-1");

    let mut flipped = serde_json::json!({"system": STYLED, "messages": []});
    apply_system_holds(&state, &mut flipped, "sess-1", "req-2");
    assert_eq!(flipped["system"], STYLED, "no pin should have been latched");
}

/// An unheld turn must come back byte-identical rather than
/// re-serialized, or a turn no hold touched could pick up a
/// formatting difference and break the prefix by itself.
#[test]
fn an_unheld_body_is_passed_through_byte_for_byte() {
    let state = crate::test_support::test_state(|c| {
        c.prefix_replay = true;
        c.hold_role_sentence = true;
    });
    let body = bytes::Bytes::from_static(br#"{"system":"Be helpful.","messages":[]}"#);
    let out = apply_system_holds_to_bytes(&state, body.clone(), "sess-1", "req-1");
    assert_eq!(out, body);
}

/// The footprint moved to the blocking pool; the tracker must not notice.
#[tokio::test]
async fn spawned_footprint_records_what_the_inline_call_did() {
    let original = bytes::Bytes::from_static(
            br#"{"system":"s","tools":[{"name":"read","description":"r"}],"messages":[{"role":"user","content":[{"type":"tool_use","id":"t1","name":"read","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}]}"#,
        );
    let on_the_wire = bytes::Bytes::from_static(
            br#"{"system":"s plus injected text","tools":[{"name":"read","description":"r"},{"name":"memory_search","description":"m"}],"messages":[{"role":"user","content":[{"type":"tool_use","id":"t1","name":"read","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}]}"#,
        );
    // A `None` path loads the live state file, which the running proxy
    // rewrites between two loads; each tracker gets its own empty file.
    let dir = tempfile::tempdir().unwrap();
    let fresh = |name: &str| {
        headroom_core::savings_tracker::SavingsTracker::new(Some(dir.path().join(name)), false)
    };
    let inline = fresh("inline.json");
    record_request_footprint(&inline, "req-inline", &original, &on_the_wire);

    let spawned = Arc::new(fresh("spawned.json"));
    spawn_request_footprint(
        spawned.clone(),
        "req-spawned".to_string(),
        original.clone(),
        on_the_wire.clone(),
    )
    .await
    .unwrap();

    assert_eq!(
        inline.proxy_overhead_report(),
        spawned.proxy_overhead_report()
    );
    assert_eq!(
        inline.tool_inventory_report(),
        spawned.tool_inventory_report()
    );
    assert_ne!(
        inline.proxy_overhead_report(),
        fresh("untouched.json").proxy_overhead_report(),
        "the fixture must move a counter or the comparison proves nothing"
    );
}

/// End-to-end unit test for `handle_ccr_response` on the OpenAI Responses
/// shape: a `function_call` for `headroom_retrieve` in the upstream
/// `output[]` must be intercepted server-side, resolved against the CCR
/// store, and a continuation request re-sent (with `input[]` extended by
/// the assistant output items + `function_call_output` items) whose reply
/// is returned to the client. Mirrors the Anthropic CCR interception path.
#[tokio::test]
async fn handle_ccr_response_openai_responses_runs_continuation() {
    use headroom_core::ccr::CcrStore;
    use headroom_core::ccr::backends::InMemoryCcrStore;
    use headroom_core::ccr::tool_injection::CCR_TOOL_NAME;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Store some original content the model will retrieve.
    let store = InMemoryCcrStore::new();
    let hash = "abc123def456abc123def456";
    store.put(hash, "the original large content");

    // Mock upstream: the continuation call returns a plain Responses reply
    // with no further CCR calls, so the loop terminates after one round.
    let server = MockServer::start().await;
    // Carries usage on purpose: the caller parses the returned body for
    // its own accounting, so counting it here too would double-bill it.
    let final_body = serde_json::json!({
        "output": [
            {"type": "message", "content": [{"type": "output_text", "text": "done"}]}
        ],
        "usage": {"input_tokens": 7_777, "output_tokens": 99}
    });
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(final_body.clone()))
        .mount(&server)
        .await;

    // The forwarded request (Responses shape uses `input[]`).
    let forwarded_request = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "gpt-x",
            "input": [{"type": "message", "role": "user", "content": "hi"}]
        }))
        .unwrap(),
    );

    // Upstream's first reply: a headroom_retrieve function_call. It
    // carries a usage block, because that first call was billed and the
    // client will never see it.
    let upstream_reply = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "output": [
                {"type": "function_call", "call_id": "call_1", "name": CCR_TOOL_NAME,
                 "arguments": format!("{{\"hash\":\"{hash}\"}}")}
            ],
            "usage": {
                "input_tokens": 4_000,
                "output_tokens": 60,
                "cache_read_input_tokens": 30_000,
                "cache_creation_input_tokens": 500
            }
        }))
        .unwrap(),
    );

    let config = Config::for_test(server.uri().parse().unwrap());
    let upstream_url: url::Url = format!("{}/v1/responses", server.uri()).parse().unwrap();
    let client = reqwest::Client::new();
    let headers = http::HeaderMap::new();

    let out = handle_ccr_response(
        &upstream_reply,
        &forwarded_request,
        &upstream_url,
        &client,
        &store as &dyn headroom_core::ccr::CcrStore,
        None,
        &config,
        "req-test",
        &headers,
        "openai_responses",
        None,
    )
    .await;

    // The returned body is the continuation reply (no CCR calls), proving
    // interception happened rather than passing the retrieve call through.
    let (body, round_usage) = out;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["output"][0]["content"][0]["text"], "done");

    // The intercepted round was a real billed call. The caller only ever
    // parses the body returned above, so unless these come back with it
    // they are never accounted for anywhere.
    assert_eq!(round_usage.rounds, 1);
    assert_eq!(round_usage.input_tokens, 4_000);
    assert_eq!(round_usage.output_tokens, 60);
    assert_eq!(round_usage.cache_read_tokens, 30_000);
    assert_eq!(round_usage.cache_write_tokens, 500);
    // The returned body's own usage stays out of it — the caller adds that.
    assert_eq!(parsed["usage"]["input_tokens"], 7_777);
    assert_ne!(round_usage.input_tokens, 4_000 + 7_777);
}

/// Query path: a `headroom_retrieve` call with `query` and no `hash`
/// searches the current project's content index and answers inline.
#[tokio::test]
async fn handle_ccr_response_anthropic_query_searches_content_index() {
    use headroom_core::ccr::backends::InMemoryCcrStore;
    use headroom_core::ccr::tool_injection::CCR_TOOL_NAME;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Content index with one block containing a distinctive phrase.
    let dir = tempfile::tempdir().unwrap();
    let stores = std::sync::Arc::new(crate::ctx::projects::ProjectStores::new(
        dir.path().to_path_buf(),
    ));
    stores
        .content("testproj")
        .expect("content store opens")
        .index_content(
            "notes",
            "the needle phrase lives here among ordinary words",
            &headroom_core::ctx::IndexOpts {
                plain_text_lines: Some(50),
                ..Default::default()
            },
        )
        .expect("indexing works");

    let server = MockServer::start().await;
    let final_body = serde_json::json!({
        "id": "msg_2", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "done"}],
        "model": "m", "stop_reason": "end_turn",
        "usage": {"input_tokens": 20, "output_tokens": 3}
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(final_body))
        .mount(&server)
        .await;

    let forwarded_request = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap(),
    );
    let upstream_reply = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "tu_1", "name": CCR_TOOL_NAME,
                 "input": {"query": "needle phrase"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 10}
        }))
        .unwrap(),
    );

    let config = Config::for_test(server.uri().parse().unwrap());
    let upstream_url: url::Url = format!("{}/v1/messages", server.uri()).parse().unwrap();
    let client = reqwest::Client::new();
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "x-headroom-project-id",
        http::HeaderValue::from_static("testproj"),
    );
    // The hot CCR store stays empty: the answer must come from the
    // content index, proving the query path does not need a hash.
    let ccr_store = InMemoryCcrStore::new();

    let (body, round_usage) = handle_ccr_response(
        &upstream_reply,
        &forwarded_request,
        &upstream_url,
        &client,
        &ccr_store as &dyn headroom_core::ccr::CcrStore,
        Some(&stores),
        &config,
        "req-test",
        &headers,
        "anthropic",
        None,
    )
    .await;

    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["content"][0]["text"], "done");
    assert_eq!(round_usage.rounds, 1);

    // The continuation carried the indexed content back upstream.
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    let sent_str = serde_json::to_string(&sent).unwrap();
    assert!(
        sent_str.contains("needle phrase lives here"),
        "continuation must carry the indexed hit: {sent_str}"
    );
}

/// A streamed continuation response folds back into a turn: backends that
/// mandate streaming (the chatgpt codex gateway 400s `stream: false`)
/// answer continuations with SSE, which plain JSON parsing cannot read.
#[tokio::test]
async fn handle_ccr_response_openai_responses_reads_sse_continuation() {
    use headroom_core::ccr::CcrStore;
    use headroom_core::ccr::backends::InMemoryCcrStore;
    use headroom_core::ccr::tool_injection::CCR_TOOL_NAME;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let store = InMemoryCcrStore::new();
    let hash = "abc123def456abc123def456";
    store.put(hash, "the original large content");

    // The continuation went out streamed and comes back SSE.
    let server = MockServer::start().await;
    let sse = "event: response.output_item.done\n\
                   data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n\
                   \n\
                   event: response.completed\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}],\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\
                   \n";
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&server)
        .await;

    let forwarded_request = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "gpt-x",
            "stream": true,
            "input": [{"type": "message", "role": "user", "content": "hi"}]
        }))
        .unwrap(),
    );
    let upstream_reply = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "output": [
                {"type": "function_call", "call_id": "call_1", "name": CCR_TOOL_NAME,
                 "arguments": format!("{{\"hash\":\"{hash}\"}}")}
            ],
            "usage": {"input_tokens": 4_000, "output_tokens": 60}
        }))
        .unwrap(),
    );

    let config = Config::for_test(server.uri().parse().unwrap());
    let upstream_url: url::Url = format!("{}/v1/responses", server.uri()).parse().unwrap();
    let client = reqwest::Client::new();
    let headers = http::HeaderMap::new();

    let (body, round_usage) = handle_ccr_response(
        &upstream_reply,
        &forwarded_request,
        &upstream_url,
        &client,
        &store as &dyn headroom_core::ccr::CcrStore,
        None,
        &config,
        "req-test-sse",
        &headers,
        "openai_responses",
        None,
    )
    .await;

    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        parsed["output"][0]["content"][0]["text"], "done",
        "the SSE continuation must resolve like a JSON one: {parsed}"
    );
    assert_eq!(round_usage.rounds, 1);
}

/// Cut-stream retry: a 200 continuation whose SSE body ends with no
/// terminal event (reasoning deltas then EOF — the 2026-09-17 luna
/// shape, ~200 KB received, zero output blocks) is resent same-round
/// instead of falling back to a splice. The retry resolving proves the
/// turn answers instead of going quiet holding unanswered content.
#[tokio::test]
async fn handle_ccr_response_openai_responses_retries_cut_continuation() {
    use headroom_core::ccr::CcrStore;
    use headroom_core::ccr::backends::InMemoryCcrStore;
    use headroom_core::ccr::tool_injection::CCR_TOOL_NAME;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let store = InMemoryCcrStore::new();
    let hash = "abc123def456abc123def456";
    store.put(hash, "the original large content");

    let server = MockServer::start().await;
    let cut_sse = "event: response.created\n\
                       data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_cut\",\"status\":\"in_progress\"}}\n\
                       \n\
                       event: response.reasoning_summary_text.delta\n\
                       data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"delta\":\"thinking about caches\"}\n\
                       \n";
    let good_sse = "event: response.output_item.done\n\
                       data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n\
                       \n\
                       event: response.completed\n\
                       data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}],\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\
                       \n";
    let cut_sse = cut_sse.to_string();
    let good_sse = good_sse.to_string();
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |_: &Request| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200).set_body_raw(cut_sse.clone(), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_raw(good_sse.clone(), "text/event-stream")
            }
        })
        .mount(&server)
        .await;

    let forwarded_request = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "gpt-x",
            "stream": true,
            "input": [{"type": "message", "role": "user", "content": "hi"}]
        }))
        .unwrap(),
    );
    let upstream_reply = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "output": [
                {"type": "function_call", "call_id": "call_1", "name": CCR_TOOL_NAME,
                 "arguments": format!("{{\"hash\":\"{hash}\"}}")}
            ],
            "usage": {"input_tokens": 4_000, "output_tokens": 60}
        }))
        .unwrap(),
    );

    let config = Config::for_test(server.uri().parse().unwrap());
    let upstream_url: url::Url = format!("{}/v1/responses", server.uri()).parse().unwrap();
    let client = reqwest::Client::new();
    let headers = http::HeaderMap::new();

    let (body, round_usage) = handle_ccr_response(
        &upstream_reply,
        &forwarded_request,
        &upstream_url,
        &client,
        &store as &dyn headroom_core::ccr::CcrStore,
        None,
        &config,
        "req-test-cut",
        &headers,
        "openai_responses",
        None,
    )
    .await;

    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        parsed["output"][0]["content"][0]["text"], "done",
        "the retry must resolve the turn instead of splicing: {parsed}"
    );
    assert!(
        !serde_json::to_string(&parsed)
            .unwrap()
            .contains("retrieved_context"),
        "no in-place splice when the retry answers: {parsed}"
    );
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 2, "cut body then retried re-send: {parsed}");
    // Both upstream calls were billed: the cut body and its retry.
    assert_eq!(round_usage.rounds, 2);
}

/// Same-project recovery on the model path: a block indexed under the
/// requesting project but expired from the CCR store must resolve via
/// the own-project fast path (the cross-project sweep skips it by
/// design), through a normal continuation — no splice, no stall.
#[tokio::test]
async fn handle_ccr_response_recovers_same_project_block() {
    use headroom_core::ccr::backends::InMemoryCcrStore;
    use headroom_core::ccr::tool_injection::CCR_TOOL_NAME;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Hot tier deliberately empty: the block lives only in alpha's
    // content index (post-TTL shape).
    let ccr_store = InMemoryCcrStore::new();
    let hash = "abc123def456abc123def456";
    let dir = tempfile::tempdir().unwrap();
    let stores = std::sync::Arc::new(crate::ctx::projects::ProjectStores::new(
        dir.path().to_path_buf(),
    ));
    stores
        .content("/home/dev/alpha")
        .expect("content store opens")
        .index_content(
            "the tool call that produced it",
            "alpha's original block content",
            &headroom_core::ctx::IndexOpts {
                content_hash: Some(hash.to_string()),
                plain_text_lines: Some(50),
                ..Default::default()
            },
        )
        .expect("index write");

    let server = MockServer::start().await;
    let final_body = serde_json::json!({
        "output": [
            {"type": "message", "content": [{"type": "output_text", "text": "done"}]}
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(final_body))
        .mount(&server)
        .await;

    let forwarded_request = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "gpt-x",
            "input": [{"type": "message", "role": "user", "content": "hi"}]
        }))
        .unwrap(),
    );
    let upstream_reply = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "output": [
                {"type": "function_call", "call_id": "call_1", "name": CCR_TOOL_NAME,
                 "arguments": format!("{{\"hash\":\"{hash}\"}}")}
            ],
            "usage": {"input_tokens": 100, "output_tokens": 10}
        }))
        .unwrap(),
    );

    let config = Config::for_test(server.uri().parse().unwrap());
    let upstream_url: url::Url = format!("{}/v1/responses", server.uri()).parse().unwrap();
    let client = reqwest::Client::new();
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "x-headroom-cwd",
        http::HeaderValue::from_static("/home/dev/alpha"),
    );

    let local_before = crate::observability::ccr_retrieval::local_tier_hits_get();
    let cross_before = crate::observability::ccr_retrieval::cross_project_hits_get();
    let (body, _) = handle_ccr_response(
        &upstream_reply,
        &forwarded_request,
        &upstream_url,
        &client,
        &ccr_store as &dyn headroom_core::ccr::CcrStore,
        Some(&stores),
        &config,
        "req-test-local",
        &headers,
        "openai_responses",
        None,
    )
    .await;

    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        parsed["output"][0]["content"][0]["text"], "done",
        "same-project recovery must continue, not splice: {parsed}"
    );
    assert!(
        !serde_json::to_string(&parsed)
            .unwrap()
            .contains("retrieved_context"),
        "recovered content goes to the continuation, not the client: {parsed}"
    );

    // And the continuation carried the recovered block upstream.
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    assert!(
        serde_json::to_string(&sent)
            .unwrap()
            .contains("alpha's original block content"),
        "continuation must carry the recovered block: {sent}"
    );
    assert_eq!(
        crate::observability::ccr_retrieval::local_tier_hits_get() - local_before,
        1,
        "same-project recovery books the local tier, not the cross-project counter"
    );
    assert_eq!(
        crate::observability::ccr_retrieval::cross_project_hits_get() - cross_before,
        0,
        "no sweep needed when the requesting project's own store answers"
    );
}

/// Unit coverage for the continuation body reader: JSON passes through
/// untouched, SSE folds only for the Responses shape, garbage stays loud.
#[test]
fn continuation_turn_from_body_reads_json_then_sse() {
    let json = bytes::Bytes::from(r#"{"output":[{"type":"message"}]}"#);
    let v = continuation_turn_from_body(&json, Some("text/event-stream"), "openai_responses")
        .expect("JSON parses regardless of content type");
    assert_eq!(v["output"][0]["type"], "message");

    let sse = bytes::Bytes::from(
        "event: response.completed\n\
             data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
    );
    let v = continuation_turn_from_body(&sse, Some("text/event-stream"), "openai_responses")
        .expect("Responses SSE folds into a turn");
    assert_eq!(v["output"][0]["content"][0]["text"], "hi");

    assert!(
        continuation_turn_from_body(&sse, Some("text/event-stream"), "anthropic").is_none(),
        "a Responses body carries no Anthropic events to fold"
    );

    let anthropic_sse = bytes::Bytes::from(
        "event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\",\"usage\":{\"input_tokens\":7,\"output_tokens\":0}}}\n\n\
             event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
             event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
             event: content_block_stop\n\
             data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n\
             event: message_stop\n\
             data: {\"type\":\"message_stop\"}\n\n",
    );
    let v = continuation_turn_from_body(&anthropic_sse, Some("text/event-stream"), "anthropic")
        .expect("Anthropic SSE folds into a turn");
    assert_eq!(v["content"][0]["text"], "hi");
    assert_eq!(v["stop_reason"], "end_turn");
    assert_eq!(v["usage"]["input_tokens"], 7);
    assert_eq!(v["usage"]["output_tokens"], 3);
    assert!(
        continuation_turn_from_body(&anthropic_sse, Some("application/json"), "anthropic")
            .is_none(),
        "a content type that says JSON still wins over the sniff"
    );
    let garbage = bytes::Bytes::from("data: not json\n\n");
    assert!(
        continuation_turn_from_body(&garbage, Some("text/event-stream"), "openai_responses")
            .is_none(),
        "SSE garbage must fail loudly, not resolve into an empty turn"
    );
    assert!(
        continuation_turn_from_body(&garbage, Some("application/json"), "openai_responses")
            .is_none(),
        "non-SSE garbage has no fold to try"
    );

    // No Content-Type at all: sniff the body instead of refusing.
    let v = continuation_turn_from_body(&sse, None, "openai_responses")
        .expect("SSE-shaped body with no Content-Type still folds");
    assert_eq!(v["output"][0]["content"][0]["text"], "hi");
    let v = continuation_turn_from_body(&sse, Some(""), "openai_responses")
        .expect("empty Content-Type counts as absent");
    assert_eq!(v["output"][0]["content"][0]["text"], "hi");
    assert!(
        continuation_turn_from_body(&sse, Some("application/json"), "openai_responses").is_none(),
        "a wrong Content-Type still wins over the sniff"
    );
    assert!(
        continuation_turn_from_body(&bytes::Bytes::from("<html>"), None, "openai_responses")
            .is_none(),
        "non-SSE body with no Content-Type has no fold to try"
    );
}

/// Terminal classification for the cut-continuation retry: explicit
/// verdicts must not retry, a missing terminal on an SSE body must.
#[test]
fn continuation_stream_terminal_classifies_cut_vs_verdict() {
    // The 2026-09-17 incident: ~200 KB of reasoning deltas, EOF, no
    // terminal event. Folds to nothing and must read as a cut stream.
    let cut = bytes::Bytes::from(
        "event: response.created\n\
             data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n\
             event: response.reasoning_summary_text.delta\n\
             data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"delta\":\"thinking about caches\"}}\n\n",
    );
    assert_eq!(continuation_stream_terminal(&cut, "openai_responses"), None);
    assert!(
        continuation_turn_from_body(&cut, Some("text/event-stream"), "openai_responses").is_none(),
        "reasoning-only stream with no terminal folds to no blocks"
    );

    for (terminal, event) in [
        ("completed", "response.completed"),
        ("failed", "response.failed"),
        ("incomplete", "response.incomplete"),
    ] {
        let body = bytes::Bytes::from(format!(
            "event: {event}\ndata: {{\"type\":\"{event}\",\"response\":{{\"id\":\"r\"}}}}\n\n"
        ));
        assert_eq!(
            continuation_stream_terminal(&body, "openai_responses"),
            Some(terminal),
            "explicit verdicts must be named, never retried as cuts"
        );
    }

    let anthropic_cut = bytes::Bytes::from(
        "event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{}}}\n\n\
             event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    );
    assert_eq!(
        continuation_stream_terminal(&anthropic_cut, "anthropic"),
        None,
        "Anthropic stream without message_delta is a cut"
    );
    let anthropic_done = bytes::Bytes::from(
        "event: message_delta\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
    );
    assert_eq!(
        continuation_stream_terminal(&anthropic_done, "anthropic"),
        Some("message_delta")
    );

    assert_eq!(continuation_stream_terminal(&cut, "openai"), None);
    assert_eq!(continuation_stream_terminal(&cut, "google"), None);
}

/// Truncation signature for the chat-JSON cut retry: only bodies that do
/// not end like complete JSON read as cut mid-write.
#[test]
fn continuation_json_truncation_signature() {
    assert!(!continuation_json_looks_truncated(br#"{"a":1}"#));
    assert!(!continuation_json_looks_truncated(b"{\"a\":1}  \n"));
    assert!(!continuation_json_looks_truncated(br#"[1,2]"#));
    assert!(continuation_json_looks_truncated(br#"{"a":1,"#));
    assert!(continuation_json_looks_truncated(b""));
    assert!(
        continuation_json_looks_truncated(b"   "),
        "whitespace-only is never a valid turn"
    );
}

/// All retrievals failed: the loop must answer the errors in place
/// instead of paying a full-prefix continuation round to deliver them.
#[tokio::test]
async fn handle_ccr_response_skips_continuation_when_everything_failed() {
    use headroom_core::ccr::backends::InMemoryCcrStore;
    use headroom_core::ccr::tool_injection::CCR_TOOL_NAME;
    use wiremock::MockServer;

    // Empty store: the hash below is a guaranteed miss.
    let store = InMemoryCcrStore::new();
    let server = MockServer::start().await;
    // No mock mounted on purpose — any continuation POST would 404,
    // and the received-requests assertion below proves none happened.

    let forwarded_request = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "claude-x",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap(),
    );
    let upstream_reply = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "content": [
                {"type": "text", "text": "Let me retrieve that."},
                {"type": "tool_use", "id": "toolu_1", "name": CCR_TOOL_NAME,
                    "input": {"hash": "ffffffffffffffffffffffff"}},
            ],
        }))
        .unwrap(),
    );

    let config = Config::for_test(server.uri().parse().unwrap());
    let upstream_url: url::Url = format!("{}/v1/messages", server.uri()).parse().unwrap();
    let client = reqwest::Client::new();
    let headers = http::HeaderMap::new();

    let (body, round_usage) = handle_ccr_response(
        &upstream_reply,
        &forwarded_request,
        &upstream_url,
        &client,
        &store as &dyn headroom_core::ccr::CcrStore,
        None,
        &config,
        "req-test-failed",
        &headers,
        "anthropic",
        None,
    )
    .await;

    // No continuation round ran: zero billed rounds, zero upstream calls.
    assert_eq!(round_usage.rounds, 0);
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
    // The failure is answered as text, so the client sees the error
    // instead of a tool_use for a tool it never declared.
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let blocks = parsed["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[1]["type"], "text");
    assert!(
        blocks[1]["text"]
            .as_str()
            .unwrap()
            .contains("ffffffffffffffffffffffff")
    );
}

/// No retrieval, no extra rounds — the common path must report nothing so
/// the accounting is untouched.
#[tokio::test]
async fn ccr_reports_no_rounds_when_nothing_was_retrieved() {
    use headroom_core::ccr::backends::InMemoryCcrStore;
    use wiremock::MockServer;

    let store = InMemoryCcrStore::new();
    let server = MockServer::start().await;
    let plain = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "hi"}]}],
            "usage": {"input_tokens": 10, "output_tokens": 2}
        }))
        .unwrap(),
    );
    let config = Config::for_test(server.uri().parse().unwrap());
    let (_body, round_usage) = handle_ccr_response(
        &plain,
        &plain,
        &format!("{}/v1/responses", server.uri()).parse().unwrap(),
        &reqwest::Client::new(),
        &store as &dyn headroom_core::ccr::CcrStore,
        None,
        &config,
        "req-test",
        &http::HeaderMap::new(),
        "openai_responses",
        None,
    )
    .await;
    assert!(round_usage.is_empty());
    assert_eq!(round_usage.input_tokens, 0);
}

#[test]
fn rejected_item_summary_names_the_item_the_400_points_at() {
    let request = serde_json::json!({"input": [
        {"role": "user", "content": "hi"},
        {"role": "tool", "tool_call_id": "c", "content": "x".repeat(200)},
    ]});
    let detail =
        r#"{"error":{"param":"input[1]","message":"`input[1]` did not match any supported type"}}"#;
    let out = rejected_item_summary(detail, &request, "input");
    assert!(out.starts_with("input[1]={"), "{out}");
    assert!(out.contains("\"role\":\"tool\""), "{out}");
    assert!(out.len() < 200, "long strings are cut: {out}");
    assert_eq!(
        rejected_item_summary("no index here", &request, "input"),
        "-"
    );
    assert_eq!(
        rejected_item_summary("input[9] bad", &request, "input"),
        "-"
    );
}

#[test]
fn extend_or_push_splices_sentinel_and_pushes_plain() {
    let mut items = vec![serde_json::json!({"role": "user"})];
    // Plain entry is pushed as one.
    extend_or_push(
        &mut items,
        serde_json::json!({"role": "assistant"}),
        &["_openai_responses_input_items"],
    );
    // Sentinel wrapper is spliced.
    extend_or_push(
        &mut items,
        serde_json::json!({"_openai_responses_tool_results": [{"a": 1}, {"a": 2}]}),
        &["_openai_tool_results", "_openai_responses_tool_results"],
    );
    assert_eq!(items.len(), 4);
    assert_eq!(items[1]["role"], "assistant");
    assert_eq!(items[2]["a"], 1);
    assert_eq!(items[3]["a"], 2);
}

#[test]
fn url_build_basic() {
    let base: url::Url = "http://up:8080".parse().unwrap();
    let uri: Uri = "/v1/messages?stream=true".parse().unwrap();
    let out = build_upstream_url(&base, &uri).unwrap();
    assert_eq!(out.as_str(), "http://up:8080/v1/messages?stream=true");
}

/// Annotation keys are billed on every turn because tools are resent
/// each request. Compaction strips them before the body goes upstream.
#[test]
fn compact_tool_schemas_strips_annotation_keys() {
    let body = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "m",
            "tools": [{
                "name": "search",
                "description": "Search  the\tweb.",
                "input_schema": {
                    "type": "object",
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "title": "SearchArgs",
                    "properties": {
                        "query": {"type": "string", "title": "Query", "examples": ["a"]}
                    }
                }
            }]
        }))
        .unwrap(),
    );
    let out = maybe_compact_tool_schemas(body.clone(), "req-test");
    assert!(out.len() < body.len(), "compaction must shrink the body");
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let schema = &v["tools"][0]["input_schema"];
    assert!(schema.get("$schema").is_none(), "$schema must be stripped");
    assert!(schema.get("title").is_none(), "title must be stripped");
    assert!(schema["properties"]["query"].get("examples").is_none());
    // Non-tool fields are untouched.
    assert_eq!(v["model"], "m");
}

/// A request with nothing to strip must forward the ORIGINAL bytes, not a
/// re-serialized equivalent — re-serializing perturbs the cache prefix.
#[test]
fn compact_tool_schemas_is_byte_identical_passthrough_when_clean() {
    for payload in [
        serde_json::json!({"model": "m", "messages": []}),
        serde_json::json!({
            "model": "m",
            "tools": [{
                "name": "x",
                "description": "Clean desc.",
                "input_schema": {"type": "object"}
            }]
        }),
    ] {
        let original = bytes::Bytes::from(serde_json::to_vec(&payload).unwrap());
        let out = maybe_compact_tool_schemas(original.clone(), "req-test");
        assert_eq!(out, original, "clean body must pass through byte-identical");
    }
}

#[test]
fn compact_tool_schemas_passthrough_on_unparseable_body() {
    let original = bytes::Bytes::from(b"not json at all".to_vec());
    assert_eq!(
        maybe_compact_tool_schemas(original.clone(), "req-test"),
        original
    );
}

fn outcome_ctx_for_sizes(original_tokens: i64, tokens_saved: i64) -> OutcomeContext {
    OutcomeContext {
        sink: Arc::new(ProxyOutcomeSink {
            cost_tracker: Arc::new(headroom_core::cost_tracker::CostTracker::new(
                None, "monthly",
            )),
            savings_tracker: Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
                None, false,
            )),
            request_logger: Arc::new(crate::request_logger::RequestLogger::new(None)),
        }),
        model: "m".into(),
        provider: "anthropic".into(),
        tags: Default::default(),
        client: None,
        project: None,
        original_tokens,
        tokens_saved,
        transforms_applied: vec![],
        num_messages: 0,
        total_latency_ms: 0.0,
        overhead_ms: 0.0,
        started_at: Instant::now(),
        waste_signals: None,
        proactive_expansion_applied: false,
        wire_bytes: None,
        forwarded_tokens_estimate: 0,
        upstream_attempts: 1,
        conversation_key: None,
    }
}

#[test]
fn proactive_expansion_cache_write_is_attributed_only_to_injected_requests() {
    let registry = crate::observability::prometheus::registry();
    let before =
        crate::observability::ctx_metrics::proactive_expansion_cache_write_tokens_get(registry);

    let untouched = outcome_ctx_for_sizes(0, 0);
    observe_proactive_expansion_cache_write(&untouched, 100);
    assert_eq!(
        crate::observability::ctx_metrics::proactive_expansion_cache_write_tokens_get(registry),
        before
    );

    let mut injected = outcome_ctx_for_sizes(0, 0);
    injected.proactive_expansion_applied = true;
    observe_proactive_expansion_cache_write(&injected, 100);
    assert_eq!(
        crate::observability::ctx_metrics::proactive_expansion_cache_write_tokens_get(registry),
        before + 100
    );
}

#[test]
fn forwarded_rejections_persist_each_status_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let tracker = Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
        Some(dir.path().join("proxy_savings.json")),
        false,
    ));
    let mut ctx = outcome_ctx_for_sizes(1_000, 100);
    ctx.sink = Arc::new(ProxyOutcomeSink {
        cost_tracker: Arc::new(headroom_core::cost_tracker::CostTracker::new(
            None, "monthly",
        )),
        savings_tracker: tracker.clone(),
        request_logger: Arc::new(crate::request_logger::RequestLogger::new(None)),
    });

    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        emit_failed_http_outcome(&ctx, "rejected", status, None);
    }

    let snapshot = tracker.snapshot();
    assert_eq!(snapshot["lifetime"]["requests"], 0);
    assert_eq!(snapshot["failed_work"]["requests"], 3);
    assert_eq!(snapshot["failed_work"]["by_status"]["401"], 1);
    assert_eq!(snapshot["failed_work"]["by_status"]["429"], 1);
    assert_eq!(snapshot["failed_work"]["by_status"]["503"], 1);
    let metrics = tracker.metrics_snapshot(&serde_json::json!({}));
    assert_eq!(metrics["requests"]["total"], 0);
    assert_eq!(metrics["requests"]["failed"], 3);
}

#[test]
fn billed_input_tokens_use_the_upstream_cache_usage_not_savings_baseline() {
    let outcome = headroom_core::request_outcome::RequestOutcome {
        // These are a compression comparison, not the provider bill.
        original_tokens: 100_000,
        optimized_tokens: 10_000,
        // Anthropic usage from the request that actually crossed the
        // proxy boundary: 2k uncached plus 7k cache read plus 1k write.
        uncached_input_tokens: 2_000,
        cache_read_tokens: 7_000,
        cache_write_tokens: 1_000,
        ..Default::default()
    };
    assert_eq!(provider_billed_input_tokens(&outcome), 10_000);
}

#[test]
fn billed_input_tokens_fall_back_to_the_post_transform_estimate() {
    let outcome = headroom_core::request_outcome::RequestOutcome {
        original_tokens: 100_000,
        optimized_tokens: 10_000,
        ..Default::default()
    };
    assert_eq!(provider_billed_input_tokens(&outcome), 10_000);
}

/// When compression ran, its own pre-compression size is the baseline.
#[test]
fn sizes_uses_the_compression_baseline_when_there_is_one() {
    let ctx = outcome_ctx_for_sizes(10_000, 2_000);
    // The provider's count is deliberately inconsistent here: compression
    // measured the body itself, so its numbers win.
    assert_eq!(ctx.sizes(7_500), (10_000, 8_000));
}

/// The gap this closes: ctx_offload shrinks the body outside the
/// compression pipeline, so `original_tokens` is 0 while `tokens_saved` is
/// real. Booking that against a zero baseline reported a 0% saving and
/// contributed nothing to the savings tracker.
#[test]
fn sizes_derives_a_baseline_when_compression_did_not_run() {
    let ctx = outcome_ctx_for_sizes(0, 1_500);
    // Forwarded 20k, removed 1.5k, so the body arrived at 21.5k.
    assert_eq!(ctx.sizes(20_000), (21_500, 20_000));

    let outcome = headroom_core::request_outcome::RequestOutcome {
        original_tokens: 21_500,
        tokens_saved: 1_500,
        ..Default::default()
    };
    assert!(
        (outcome.savings_pct() - 6.976_744_186_046_512).abs() < 1e-9,
        "a real saving must report a real percentage, got {}",
        outcome.savings_pct()
    );
}

/// Regression guard for items 1d/1e: the booked saving is the compression
/// dispatcher's own per-turn figure, so `tok_after` can never go negative
/// by absorbing a CTX-offload total measured against a different baseline.
///
/// The numbers are the live turn from item 1e (2026-08-08 22:40:36Z):
/// compression saw a 358-token live zone and freed 243, while the CTX
/// transforms had already removed 12,197 tokens earlier in the pipeline.
/// Folding that 12,197 into this subtraction was the original defect — it
/// reported `tok_after = 358 - 12,440 = -12,082`.
#[test]
fn sizes_books_only_the_compression_turn_so_tok_after_stays_non_negative() {
    const COMPRESSION_TOKENS_BEFORE: i64 = 358;
    const COMPRESSION_TOKENS_FREED: i64 = 243;
    const CTX_TRANSFORM_TOKENS_SAVED: i64 = 12_197;

    let ctx = outcome_ctx_for_sizes(COMPRESSION_TOKENS_BEFORE, COMPRESSION_TOKENS_FREED);
    let (original, optimized) = ctx.sizes(0);

    // The published subtraction matches the `compression applied` line's
    // own arithmetic, which is the only per-turn measurement available.
    assert_eq!(original, COMPRESSION_TOKENS_BEFORE);
    assert_eq!(
        optimized,
        COMPRESSION_TOKENS_BEFORE - COMPRESSION_TOKENS_FREED
    );
    assert!(
        optimized >= 0,
        "tok_after must not go negative, got {optimized}"
    );

    // `saturating_sub` on i64 saturates at i64::MIN, not at zero, so it is
    // not the guard it looks like. Pin the shape the defect produced so a
    // future change that folds the CTX total back in fails here loudly.
    let folded_in = outcome_ctx_for_sizes(
        COMPRESSION_TOKENS_BEFORE,
        COMPRESSION_TOKENS_FREED + CTX_TRANSFORM_TOKENS_SAVED,
    );
    assert_eq!(folded_in.sizes(0).1, -12_082);
}

/// A passthrough turn stays at zero rather than inventing a saving.
#[test]
fn sizes_reports_no_saving_for_an_untouched_body() {
    let ctx = outcome_ctx_for_sizes(0, 0);
    assert_eq!(ctx.sizes(20_000), (20_000, 20_000));
    assert_eq!(ctx.sizes(0), (0, 0));
}

#[test]
fn maybe_prune_tools_drops_and_reserializes() {
    use crate::cache_stabilization::tool_prune::PrunePolicy;
    let body = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "m",
            "tools": [
                {"name": "Read", "input_schema": {}},
                {"name": "mcp__chrome__click", "input_schema": {}}
            ]
        }))
        .unwrap(),
    );
    let policy = PrunePolicy {
        drop_mcp_servers: ["chrome"].iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let out = maybe_prune_tools(body, &policy, "req-test");
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let tools = v["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "Read");
}

/// The head is `tools` + `system` and nothing else — the parts the
/// injection stages write to and the compressors never touch. Including
/// `messages` would mix compression's savings into the overhead figure and
/// make it meaningless.
#[test]
fn prefix_head_bytes_covers_tools_and_system_only() {
    let body = serde_json::json!({
        "model": "m",
        "system": "abc",
        "tools": [{"name": "a"}],
        "messages": [{"role": "user", "content": "a very long message body"}],
    });
    let head = prefix_head_bytes(&body);
    let without_messages = serde_json::json!({
        "model": "m",
        "system": "abc",
        "tools": [{"name": "a"}],
    });
    assert_eq!(head, prefix_head_bytes(&without_messages));
    assert!(head > 0);
}

/// A body with neither is zero, not a panic.
#[test]
fn prefix_head_bytes_handles_an_empty_body() {
    assert_eq!(prefix_head_bytes(&serde_json::json!({})), 0);
}

/// The inventory has to pair definitions with the calls the model made, in
/// both provider shapes, or the never-called list is wrong.
#[test]
fn tool_inventory_pairs_definitions_with_calls() {
    let body = serde_json::json!({
        "tools": [
            {"name": "Read", "input_schema": {"type": "object"}},
            {"name": "Workflow", "input_schema": {"type": "object"}},
            {"type": "function", "function": {"name": "Legacy"}}
        ],
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "1", "name": "Read", "input": {}},
                {"type": "tool_use", "id": "2", "name": "Read", "input": {}}
            ]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "1"}]}
        ]
    });
    let (defs, calls) = tool_inventory_of(&body);
    let names: Vec<&str> = defs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["Read", "Workflow", "Legacy"]);
    assert!(defs.iter().all(|(_, b)| *b > 0));
    assert_eq!(calls, vec![("Read".to_string(), 2)]);
}

/// A `tool_result` is not a call. Counting it would make every tool look
/// used and the never-called list would always be empty.
#[test]
fn tool_results_do_not_count_as_calls() {
    let body = serde_json::json!({
        "tools": [{"name": "Read"}],
        "messages": [{"role": "user", "content": [
            // Carries a `name` on purpose: the block *type* has to be what
            // excludes it, not the field happening to be absent.
            {"type": "tool_result", "tool_use_id": "1", "name": "Read", "content": "x"},
            {"type": "text", "text": "and some prose", "name": "Read"}
        ]}]
    });
    let (_, calls) = tool_inventory_of(&body);
    assert!(
        calls.is_empty(),
        "only tool_use blocks are calls, got {calls:?}"
    );
}

/// B2 end to end through the wiring function: turn one records, turn two
/// pushes a late-arriving tool to the tail. The rest of the body must
/// survive the reserialize untouched.
#[test]
fn maybe_stabilize_tool_order_replays_then_appends() {
    use crate::cache_stabilization::tool_order::ToolOrderStore;
    let store = ToolOrderStore::default();
    let body = |tools: serde_json::Value| {
        bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "claude-opus-4-8",
                "system": "s",
                "max_tokens": 64,
                "tools": tools,
            }))
            .unwrap(),
        )
    };
    let names = |b: &bytes::Bytes| {
        let v: serde_json::Value = serde_json::from_slice(b).unwrap();
        v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };

    let first = body(serde_json::json!([{"name": "a"}, {"name": "b"}]));
    let out = maybe_stabilize_tool_order(first.clone(), &store, "sess", "req-test");
    assert_eq!(out, first, "first turn only records; bytes must not move");

    let second = body(serde_json::json!([{"name": "a"}, {"name": "late"}, {"name": "b"}]));
    let out = maybe_stabilize_tool_order(second, &store, "sess", "req-test");
    assert_eq!(names(&out), ["a", "b", "late"]);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["max_tokens"], 64);
    assert_eq!(v["system"], "s");
}

/// A different model on the same session key must not inherit the other
/// model's order — the credential-derived session key is shared between a
/// main agent and its subagents.
#[test]
fn maybe_stabilize_tool_order_keys_on_model() {
    use crate::cache_stabilization::tool_order::ToolOrderStore;
    let store = ToolOrderStore::default();
    let body = |model: &str, tools: serde_json::Value| {
        bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({"model": model, "tools": tools})).unwrap(),
        )
    };
    let sub = body(
        "claude-sonnet-4-6",
        serde_json::json!([{"name": "b"}, {"name": "a"}]),
    );
    maybe_stabilize_tool_order(sub, &store, "sess", "req-test");

    let main = body(
        "claude-opus-4-8",
        serde_json::json!([{"name": "a"}, {"name": "b"}, {"name": "c"}]),
    );
    let out = maybe_stabilize_tool_order(main.clone(), &store, "sess", "req-test");
    assert_eq!(out, main, "subagent order must not leak across models");
}

/// No `tools` array — nothing to stabilize, and the body must not even be
/// reserialized.
#[test]
fn maybe_stabilize_tool_order_passthrough_without_tools() {
    use crate::cache_stabilization::tool_order::ToolOrderStore;
    let store = ToolOrderStore::default();
    let original = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({"model": "m", "messages": []})).unwrap(),
    );
    assert_eq!(
        maybe_stabilize_tool_order(original.clone(), &store, "sess", "req-test"),
        original
    );
}

/// Without a session key every conversation would share one store slot and
/// replay each other's tool order. Passthrough instead.
#[test]
fn maybe_stabilize_tool_order_needs_a_session_key() {
    use crate::cache_stabilization::tool_order::ToolOrderStore;
    let store = ToolOrderStore::default();
    let body = |tools: serde_json::Value| {
        bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({"model": "m", "tools": tools})).unwrap(),
        )
    };
    maybe_stabilize_tool_order(
        body(serde_json::json!([{"name": "a"}, {"name": "b"}])),
        &store,
        "",
        "req-test",
    );
    let shuffled = body(serde_json::json!([{"name": "b"}, {"name": "a"}]));
    assert_eq!(
        maybe_stabilize_tool_order(shuffled.clone(), &store, "", "req-test"),
        shuffled
    );
}

#[test]
fn maybe_prune_tools_passthrough_when_no_tools_field() {
    use crate::cache_stabilization::tool_prune::PrunePolicy;
    let original = bytes::Bytes::from(br#"{"model":"m","messages":[]}"#.to_vec());
    let policy = PrunePolicy {
        drop_mcp_servers: ["chrome"].iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let out = maybe_prune_tools(original.clone(), &policy, "req-test");
    assert_eq!(out, original, "no tools[] -> byte-identical passthrough");
}

#[test]
fn maybe_prune_tools_passthrough_when_nothing_removed() {
    use crate::cache_stabilization::tool_prune::PrunePolicy;
    let original = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "tools": [{"name": "Read", "input_schema": {}}]
        }))
        .unwrap(),
    );
    let policy = PrunePolicy {
        drop_mcp_servers: ["chrome"].iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let out = maybe_prune_tools(original.clone(), &policy, "req-test");
    assert_eq!(
        out, original,
        "nothing matched -> byte-identical passthrough"
    );
}

#[test]
fn url_build_with_base_path() {
    let base: url::Url = "http://up:8080/api".parse().unwrap();
    let uri: Uri = "/v1/messages".parse().unwrap();
    let out = build_upstream_url(&base, &uri).unwrap();
    assert_eq!(out.as_str(), "http://up:8080/api/v1/messages");
}

#[test]
fn url_build_root() {
    let base: url::Url = "http://up:8080/".parse().unwrap();
    let uri: Uri = "/".parse().unwrap();
    let out = build_upstream_url(&base, &uri).unwrap();
    assert_eq!(out.as_str(), "http://up:8080/");
}

// ── Phase 3: request_has_messages (CompressionDecision `has_messages`) ──

use compression::CompressibleEndpoint;

#[test]
fn has_messages_true_for_nonempty_anthropic_messages() {
    let body = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
    assert!(request_has_messages(
        body,
        CompressibleEndpoint::AnthropicMessages
    ));
    assert!(request_has_messages(
        body,
        CompressibleEndpoint::OpenAiChatCompletions
    ));
}

#[test]
fn has_messages_false_for_empty_messages_array() {
    let body = br#"{"messages":[]}"#;
    assert!(!request_has_messages(
        body,
        CompressibleEndpoint::AnthropicMessages
    ));
}

#[test]
fn has_messages_false_when_field_missing() {
    let body = br#"{"model":"m"}"#;
    assert!(!request_has_messages(
        body,
        CompressibleEndpoint::AnthropicMessages
    ));
}

#[test]
fn has_messages_uses_input_field_for_responses() {
    let body = br#"{"input":[{"role":"user","content":"hi"}]}"#;
    assert!(request_has_messages(
        body,
        CompressibleEndpoint::OpenAiResponses
    ));
    // `messages` on a Responses body is not the field consulted.
    let wrong = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
    assert!(!request_has_messages(
        wrong,
        CompressibleEndpoint::OpenAiResponses
    ));
}

#[test]
fn has_messages_false_on_parse_failure() {
    assert!(!request_has_messages(
        b"not json",
        CompressibleEndpoint::AnthropicMessages
    ));
}

#[test]
fn ccr_workspace_project_id_wins() {
    let mut headers = HeaderMap::new();
    headers.insert("x-headroom-project-id", "my-project".parse().unwrap());
    let body = serde_json::json!({});

    let (key, label) = resolve_ccr_workspace(Some(&headers), &body, None).unwrap();
    assert_eq!(key, "my-project");
    assert_eq!(label.as_deref(), Some("my-project"));
}

#[test]
fn ccr_workspace_configured_project_root_override() {
    // Ports upstream #3606: the CLI project-root override reaches CCR
    // workspace resolution for clients without cwd metadata.
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": "hi"}]
    });

    let (key, label) =
        resolve_ccr_workspace(None, &body, Some("/home/user/code/project-c")).unwrap();
    assert!(key.starts_with("project-c-"));
    assert_eq!(label.as_deref(), Some("project-c"));
}

#[test]
fn ccr_workspace_empty_when_unresolved() {
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": "hi"}]
    });
    assert!(resolve_ccr_workspace(None, &body, None).is_none());
}

#[test]
fn ccr_workspace_system_prompt_cwd_fallback() {
    let body = serde_json::json!({
        "system": "You are helpful.\ncwd: /home/user/code/my-project\n",
        "messages": []
    });

    let (key, label) = resolve_ccr_workspace(None, &body, None).unwrap();
    assert!(key.starts_with("my-project-"));
    assert_eq!(label.as_deref(), Some("my-project"));
}

#[test]
fn latest_user_query_reads_latest_text_block() {
    let body = serde_json::json!({
        "messages": [
            {"role": "user", "content": "old"},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": [
                {"type": "image", "source": {}},
                {"type": "text", "text": "new query"}
            ]}
        ]
    });

    assert_eq!(latest_user_query(&body), "new query");
}

#[test]
fn append_context_adds_text_block_to_latest_user_only() {
    let mut body = serde_json::json!({
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "old"}]},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": [{"type": "text", "text": "new"}]}
        ]
    });

    assert!(append_context_to_latest_user_turn(
        &mut body,
        "expanded".to_string()
    ));
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["content"].as_array().unwrap().len(), 1);
    let latest_blocks = messages[2]["content"].as_array().unwrap();
    assert_eq!(latest_blocks.len(), 2);
    assert_eq!(latest_blocks[1]["text"], "expanded");
}

#[test]
fn ccr_context_tracker_filters_cross_workspace() {
    let mut tracker = headroom_core::ccr::context_tracker::ContextTracker::new(Some(
        headroom_core::ccr::context_tracker::ContextTrackerConfig {
            relevance_threshold: 0.1,
            ..Default::default()
        },
    ));
    tracker.track_compression(
        "abc123",
        1,
        Some("Bash"),
        100,
        1,
        "workspace-a",
        "find auth middleware",
        "auth_middleware.py login handler",
    );

    assert!(
        tracker
            .analyze_query("auth middleware", Some(2), "workspace-b")
            .is_empty()
    );
    let recs = tracker.analyze_query("auth middleware", Some(2), "workspace-a");
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].hash_key, "abc123");
}

// ── is_application_json ──────────────────────────────────────────

#[test]
fn is_application_json_plain() {
    let mut h = HeaderMap::new();
    h.insert("content-type", "application/json".parse().unwrap());
    assert!(is_application_json(&h));
}

#[test]
fn is_application_json_with_charset() {
    let mut h = HeaderMap::new();
    h.insert(
        "content-type",
        "application/json; charset=utf-8".parse().unwrap(),
    );
    assert!(is_application_json(&h));
}

#[test]
fn is_application_json_case_insensitive() {
    let mut h = HeaderMap::new();
    h.insert("content-type", "Application/JSON".parse().unwrap());
    assert!(is_application_json(&h));
}

#[test]
fn is_application_json_missing_header() {
    let h = HeaderMap::new();
    assert!(!is_application_json(&h));
}

#[test]
fn is_application_json_wrong_type() {
    let mut h = HeaderMap::new();
    h.insert("content-type", "text/plain".parse().unwrap());
    assert!(!is_application_json(&h));
}

// ── is_websocket_upgrade ─────────────────────────────────────────

#[test]
fn is_websocket_upgrade_both_headers() {
    let mut h = HeaderMap::new();
    h.insert("upgrade", "websocket".parse().unwrap());
    h.insert("connection", "Upgrade".parse().unwrap());
    assert!(is_websocket_upgrade(&h));
}

#[test]
fn is_websocket_upgrade_missing_upgrade_header() {
    let mut h = HeaderMap::new();
    h.insert("connection", "Upgrade".parse().unwrap());
    assert!(!is_websocket_upgrade(&h));
}

#[test]
fn is_websocket_upgrade_missing_connection_header() {
    let mut h = HeaderMap::new();
    h.insert("upgrade", "websocket".parse().unwrap());
    assert!(!is_websocket_upgrade(&h));
}

#[test]
fn is_websocket_upgrade_connection_with_other_tokens() {
    let mut h = HeaderMap::new();
    h.insert("upgrade", "websocket".parse().unwrap());
    h.insert("connection", "keep-alive, Upgrade".parse().unwrap());
    assert!(is_websocket_upgrade(&h));
}

// ── rewritten_message_report ─────────────────────────────────────

#[test]
fn cache_control_placement_is_not_a_rewrite() {
    // The proxy re-places the breakpoint every turn by design; counting
    // that as a rewrite would mark every message and say nothing.
    let before = serde_json::json!({"role": "user", "content": [{"type": "text", "text": "hi"}]});
    let after = serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]});
    assert!(
        rewritten_message_report(&[before], &[after])
            .indices
            .is_empty()
    );
}

#[test]
fn compressed_text_beside_a_thinking_block_is_flagged() {
    let before = serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "…", "signature": "sig"},
            {"type": "text", "text": "a long log line"}]});
    let after = serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "…", "signature": "sig"},
            {"type": "text", "text": "[compressed]"}]});
    let report = rewritten_message_report(&[before], &[after]);
    assert_eq!(report.indices, vec![0]);
    assert_eq!(report.with_thinking, vec![0]);
}

#[test]
fn a_rewrite_without_thinking_is_not_flagged() {
    let before = serde_json::json!({"role": "user", "content": [
            {"type": "tool_result", "content": "a long log line"}]});
    let after = serde_json::json!({"role": "user", "content": [
            {"type": "tool_result", "content": "[compressed]"}]});
    let report = rewritten_message_report(&[before], &[after]);
    assert_eq!(report.indices, vec![0]);
    assert!(report.with_thinking.is_empty());
}

#[test]
fn stripping_cache_control_off_a_signed_block_counts_as_touching_it() {
    // The canonical compare is blind here on purpose, so this is the only
    // list that can catch it. The provider judges the block as sent.
    let before = serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "…", "signature": "sig",
             "cache_control": {"type": "ephemeral"}}]});
    let after = serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "…", "signature": "sig"}]});
    let report = rewritten_message_report(&[before], &[after]);
    assert!(report.indices.is_empty());
    assert_eq!(report.thinking_touched, vec![0]);
}

#[test]
fn an_untouched_signed_block_is_not_reported() {
    let msg = serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "…", "signature": "sig"},
            {"type": "text", "text": "hello"}]});
    let report = rewritten_message_report(std::slice::from_ref(&msg), std::slice::from_ref(&msg));
    assert!(report.thinking_touched.is_empty());
}

#[test]
fn index_lists_are_capped() {
    let many: Vec<usize> = (0..25).collect();
    assert_eq!(join_indices(&many[..3]), "0,1,2");
    assert!(join_indices(&many).ends_with("…+5"));
}

// ── describe_upstream_error ──────────────────────────────────────

#[test]
fn describes_an_anthropic_rejection() {
    let body = br#"{"type":"error","error":{"type":"invalid_request_error",
            "message":"messages.11: unexpected block"}}"#;
    let (kind, message) = describe_upstream_error(body);
    assert_eq!(kind, "invalid_request_error");
    assert_eq!(message, "messages.11: unexpected block");
}

#[test]
fn describes_an_openai_rejection() {
    let body = br#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#;
    let (kind, message) = describe_upstream_error(body);
    assert_eq!(kind, "context_length_exceeded");
    assert_eq!(message, "too long");
}

#[test]
fn unknown_error_shapes_reach_the_log_as_nothing() {
    // The point of the helper: a body the proxy does not recognise must not
    // be forwarded into the log verbatim.
    let (kind, message) = describe_upstream_error(b"<html>secret</html>");
    assert_eq!(kind, "unparsed");
    assert!(message.is_empty());
    let (kind, message) = describe_upstream_error(br#"{"detail":"secret"}"#);
    assert_eq!(kind, "no_error_field");
    assert!(message.is_empty());
}

#[test]
fn long_error_messages_are_truncated() {
    let long = "x".repeat(2_000);
    let body = format!(r#"{{"error":{{"type":"e","message":"{long}"}}}}"#);
    let (_, message) = describe_upstream_error(body.as_bytes());
    assert_eq!(message.chars().count(), 400);
}

// ── anthropic_cache_ttl_split ────────────────────────────────────
#[test]
fn cache_ttl_split_reads_the_nested_cache_creation_object() {
    let usage = serde_json::json!({
        "input_tokens": 12,
        "cache_creation_input_tokens": 4_000,
        "cache_creation": {
            "ephemeral_5m_input_tokens": 1_000,
            "ephemeral_1h_input_tokens": 3_000
        }
    });
    assert_eq!(anthropic_cache_ttl_split(Some(&usage)), (1_000, 3_000));
}

#[test]
fn cache_ttl_split_is_zero_when_the_provider_omits_it() {
    // OpenAI shapes, and older Anthropic bodies, carry no nested object.
    // Pricing treats (0, 0) as "unreported" and falls back to the 5m rate
    // rather than inventing a 1h premium.
    let usage = serde_json::json!({"prompt_tokens": 10, "completion_tokens": 2});
    assert_eq!(anthropic_cache_ttl_split(Some(&usage)), (0, 0));
    assert_eq!(anthropic_cache_ttl_split(None), (0, 0));
}

// ── is_sse_response ──────────────────────────────────────────────

#[test]
fn is_sse_response_plain() {
    let mut h = HeaderMap::new();
    h.insert("content-type", "text/event-stream".parse().unwrap());
    assert!(is_sse_response(&h));
}

#[test]
fn is_sse_response_with_charset() {
    let mut h = HeaderMap::new();
    h.insert(
        "content-type",
        "text/event-stream; charset=utf-8".parse().unwrap(),
    );
    assert!(is_sse_response(&h));
}

#[test]
fn is_sse_response_missing() {
    let h = HeaderMap::new();
    assert!(!is_sse_response(&h));
}

#[test]
fn is_sse_response_wrong_type() {
    let mut h = HeaderMap::new();
    h.insert("content-type", "application/json".parse().unwrap());
    assert!(!is_sse_response(&h));
}

// ── append_anthropic_beta ────────────────────────────────────────

#[test]
fn append_anthropic_beta_to_empty() {
    let mut h = HeaderMap::new();
    append_anthropic_beta(&mut h, "prompt-caching-2024-07-31");
    assert_eq!(
        h.get("anthropic-beta").unwrap().to_str().unwrap(),
        "prompt-caching-2024-07-31"
    );
}

/// Tail-anchored counterpart to the head ladder: the head checkpoints stop
/// doubling at 32, so on a long turn the disputed tail sits past every one
/// of them. These windows cover the last 1/2/4 forwarded messages instead.
fn ladder_body(texts: &[&str]) -> Vec<u8> {
    let messages: Vec<serde_json::Value> = texts
        .iter()
        .map(|t| serde_json::json!({"role": "user", "content": t}))
        .collect();
    serde_json::to_vec(&serde_json::json!({"messages": messages})).unwrap()
}

fn ladder_map(s: &str) -> std::collections::HashMap<String, String> {
    s.split(',')
        .filter_map(|p| p.split_once(':'))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn tail_ladder_keeps_a_fixed_schema_and_is_deterministic() {
    let body = ladder_body(&["a", "b", "c", "d", "e"]);
    let first = tail_digest_ladder(&body).unwrap();
    let first_map = ladder_map(&first);
    let mut keys: Vec<&str> = first_map.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["t1", "t2", "t4"]);
    assert_eq!(first, tail_digest_ladder(&body).unwrap());
    // Short bodies clamp the windows rather than dropping keys.
    let short = tail_digest_ladder(&ladder_body(&["a"])).unwrap();
    assert_eq!(ladder_map(&short).len(), 3);
    // Non-JSON is None, like the head ladder.
    assert_eq!(tail_digest_ladder(b"not json"), None);
}

/// The blind spot this exists for: on a 6-message turn the head ladder's
/// checkpoints (1, 2, 4) all sit at or before the tail, so a last-message
/// edit moves nothing on it — while the tail windows catch it.
#[test]
fn tail_ladder_moves_where_the_head_ladder_cannot_see() {
    let before = ladder_body(&["m0", "m1", "m2", "m3", "m4", "m5"]);
    let mut parsed: serde_json::Value = serde_json::from_slice(&before).unwrap();
    parsed["messages"][5]["content"] = serde_json::json!("m5 EDITED");
    let after = serde_json::to_vec(&parsed).unwrap();

    let head_before = ladder_map(&prefix_digest_ladder(&before).unwrap());
    let head_after = ladder_map(&prefix_digest_ladder(&after).unwrap());
    assert_eq!(
        head_before, head_after,
        "depths 1,2,4 all precede the edit, so the head ladder holds still"
    );

    let tail_before = ladder_map(&tail_digest_ladder(&before).unwrap());
    let tail_after = ladder_map(&tail_digest_ladder(&after).unwrap());
    assert_ne!(tail_before["t1"], tail_after["t1"]);
    assert_ne!(tail_before["t2"], tail_after["t2"]);
    assert_ne!(tail_before["t4"], tail_after["t4"]);
}

/// Gradient: an edit confined to the second-to-last message leaves the
/// last-message window alone, bounding the churn to the tail pair.
#[test]
fn tail_ladder_bounds_churn_to_the_smallest_moved_window() {
    let before = ladder_body(&["m0", "m1", "m2", "m3", "m4", "m5"]);
    let mut parsed: serde_json::Value = serde_json::from_slice(&before).unwrap();
    parsed["messages"][4]["content"] = serde_json::json!("m4 EDITED");
    let after = serde_json::to_vec(&parsed).unwrap();

    let tail_before = ladder_map(&tail_digest_ladder(&before).unwrap());
    let tail_after = ladder_map(&tail_digest_ladder(&after).unwrap());
    assert_eq!(tail_before["t1"], tail_after["t1"]);
    assert_ne!(tail_before["t2"], tail_after["t2"]);
    assert_ne!(tail_before["t4"], tail_after["t4"]);

    // And the reverse direction: a head edit must not move any tail
    // window that does not cover it.
    let mut parsed: serde_json::Value = serde_json::from_slice(&before).unwrap();
    parsed["messages"][0]["content"] = serde_json::json!("m0 EDITED");
    let head_edited = serde_json::to_vec(&parsed).unwrap();
    let tail_head_edited = ladder_map(&tail_digest_ladder(&head_edited).unwrap());
    assert_eq!(tail_before["t1"], tail_head_edited["t1"]);
    assert_eq!(tail_before["t2"], tail_head_edited["t2"]);
    assert_eq!(tail_before["t4"], tail_head_edited["t4"]);
}

#[test]
fn append_anthropic_beta_deduplicates() {
    let mut h = HeaderMap::new();
    h.insert(
        "anthropic-beta",
        "prompt-caching-2024-07-31".parse().unwrap(),
    );
    append_anthropic_beta(&mut h, "prompt-caching-2024-07-31");
    assert_eq!(
        h.get("anthropic-beta").unwrap().to_str().unwrap(),
        "prompt-caching-2024-07-31"
    );
}

#[test]
fn append_anthropic_beta_merges() {
    let mut h = HeaderMap::new();
    h.insert("anthropic-beta", "existing-beta".parse().unwrap());
    append_anthropic_beta(&mut h, "new-beta");
    assert_eq!(
        h.get("anthropic-beta").unwrap().to_str().unwrap(),
        "existing-beta,new-beta"
    );
}

/// The continuation retry log names the failure phase, so the next stall
/// is diagnosable from one line instead of needing a repro.
#[tokio::test]
async fn ccr_transport_kind_names_the_failure_phase() {
    // Unresolvable numeric host: DNS fails fast with no packets, which
    // surfaces as a connect-phase error. (Localhost TCP is filtered in
    // some sandboxes, so a connect-refused target is not hermetic here.)
    let e = reqwest::Client::new()
        .post("http://invalid.invalid/")
        .body("x")
        .send()
        .await
        .unwrap_err();
    assert_eq!(ccr_transport_kind(&e), "connect");
    assert!(
        !ccr_error_chain(&e).is_empty(),
        "the chain must carry more than the top-level message"
    );

    // Listener holds the connection open without answering: the client
    // timeout fires while still waiting for headers. Built from the
    // TLS-aware constructor like every other outbound client (see
    // `tls_client_wiring`), with only a timeout added.
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    let slow = crate::ssl_context::client_builder()
        .timeout(std::time::Duration::from_millis(100))
        .build()
        .unwrap();
    let e = slow.post(server.uri()).body("x").send().await.unwrap_err();
    assert_eq!(ccr_transport_kind(&e), "timeout");
}

#[test]
fn hidden_ccr_continuation_does_not_become_next_client_cache_baseline() {
    use crate::cache_stabilization::usage_observer::UsageObserver;

    let mut usage = CcrRoundUsage::default();
    usage.add_response(&serde_json::json!({
        "usage": {
            "input_tokens": 1_025,
            "cache_read_input_tokens": 92_100,
            "cache_creation_input_tokens": 1_025,
            "output_tokens": 100
        }
    }));
    let baseline = usage.client_cache_baseline(0, 92_100, 129_915);
    assert_eq!(baseline, (1_025, 92_100, 1_025));

    let observer = UsageObserver::new();
    observer.begin_request("ccr-1", "ccr-conv".into(), None, None, None);
    observer.complete("ccr-1", baseline.0, baseline.1, baseline.2, None);
    observer.begin_request("ccr-2", "ccr-conv".into(), None, None, None);
    let class = observer.complete("ccr-2", 2, 93_125, 525, None);

    assert_eq!(
        class, None,
        "93,125 exactly reuses the client-visible baseline"
    );
    assert!(observer.snapshot().last_event.is_none());
}

#[test]
fn replay_decline_logs_hashed_session_and_chain_identity() {
    use crate::cache_stabilization::drift_detector::session_key_log_prefix;
    use crate::cache_stabilization::prefix_replay::SessionReplayStore;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};

    struct Capture(Arc<Mutex<Vec<HashMap<String, String>>>>);

    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            struct Visitor(HashMap<String, String>);

            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0
                        .insert(field.name().to_string(), format!("{value:?}"));
                }

                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.insert(field.name().to_string(), value.to_string());
                }

                fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                    self.0.insert(field.name().to_string(), value.to_string());
                }
            }

            let mut visitor = Visitor(HashMap::new());
            event.record(&mut visitor);
            if visitor
                .0
                .get("event")
                .is_some_and(|name| name == "prefix_replay_not_replayed")
            {
                self.0.lock().unwrap().push(visitor.0);
            }
        }
    }

    let captured = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Capture(captured.clone()));
    let session_key = "Bearer never-log-this-session-key";
    let expected_hash = session_key_log_prefix(session_key);
    let messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
    let body = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({"messages": messages.clone()})).unwrap(),
    );

    tracing::subscriber::with_default(subscriber, || {
        apply_prefix_replay(
            &SessionReplayStore::new(2),
            session_key,
            "replay-log-test",
            messages,
            body,
            None,
            7,
            2,
            false,
        );
    });

    let captured = captured.lock().unwrap();
    let event = captured
        .first()
        .expect("first turn must emit a prefix_replay_not_replayed event");
    assert_eq!(event.get("session_key_hash"), Some(&expected_hash));
    assert_eq!(event.get("chain_id"), Some(&"0".to_string()));
    assert!(
        event.values().all(|value| !value.contains(session_key)),
        "the raw session key must never be written to the event: {event:?}"
    );
}

// ── drop_unsigned_reasoning_blocks ───────────────────────────
//
// The counterpart to `sse::stream_finisher`. These pin the two things that
// make it safe to run on every Anthropic turn: it is inert unless a stream
// actually died mid-thinking, and when it does fire it does not move the
// prompt-cache boundary.

fn body_with(messages: serde_json::Value) -> bytes::Bytes {
    bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({"model": "claude", "messages": messages})).unwrap(),
    )
}

fn messages_of(body: &bytes::Bytes) -> serde_json::Value {
    serde_json::from_slice::<serde_json::Value>(body).unwrap()["messages"].clone()
}

#[test]
fn unsigned_reasoning_drop_is_inert_without_an_unsigned_block() {
    // No reasoning at all: not even parsed, and byte-identical out.
    let plain = body_with(serde_json::json!([{"role": "user", "content": "hi"}]));
    assert_eq!(drop_unsigned_reasoning_blocks(plain.clone(), "r"), plain);

    // A signed block is a real one and must survive untouched.
    let signed = body_with(serde_json::json!([{
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "t", "signature": "sig"},
            {"type": "text", "text": "answer"},
        ],
    }]));
    assert_eq!(drop_unsigned_reasoning_blocks(signed.clone(), "r"), signed);

    // `redacted_thinking` carries `data`, never a signature.
    let redacted = body_with(serde_json::json!([{
        "role": "assistant",
        "content": [
            {"type": "redacted_thinking", "data": "opaque"},
            {"type": "text", "text": "answer"},
        ],
    }]));
    assert_eq!(
        drop_unsigned_reasoning_blocks(redacted.clone(), "r"),
        redacted
    );
}

#[test]
fn unsigned_reasoning_is_dropped_and_the_turn_stays_sendable() {
    // The shape `stream_finisher` leaves behind: thinking cut off before
    // its signature, then the truncation marker as text.
    let body = body_with(serde_json::json!([{
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "half a thought"},
            {"type": "text", "text": "[truncated: ...]"},
        ],
    }]));
    let out = drop_unsigned_reasoning_blocks(body, "r");
    let content = &messages_of(&out)[0]["content"];
    assert_eq!(content.as_array().unwrap().len(), 1);
    assert_eq!(content[0]["type"], "text");
}

#[test]
fn a_cache_breakpoint_on_a_dropped_block_moves_rather_than_vanishes() {
    // The marker sits on the doomed block. Losing it would shift the cached
    // prefix boundary and cost a re-cache on every later turn.
    let body = body_with(serde_json::json!([{
        "role": "assistant",
        "content": [
            {
                "type": "thinking",
                "thinking": "half",
                "cache_control": {"type": "ephemeral"},
            },
            {"type": "text", "text": "tail"},
        ],
    }]));
    let out = drop_unsigned_reasoning_blocks(body, "r");
    let content = messages_of(&out)[0]["content"].clone();
    assert_eq!(content.as_array().unwrap().len(), 1);
    assert_eq!(
        content[0]["cache_control"],
        serde_json::json!({"type": "ephemeral"}),
        "the breakpoint should have carried to the surviving block"
    );
}

#[test]
fn a_breakpoint_carries_backwards_when_the_dropped_block_was_last() {
    let body = body_with(serde_json::json!([{
        "role": "assistant",
        "content": [
            {"type": "text", "text": "lead"},
            {
                "type": "thinking",
                "thinking": "half",
                "cache_control": {"type": "ephemeral"},
            },
        ],
    }]));
    let out = drop_unsigned_reasoning_blocks(body, "r");
    let content = messages_of(&out)[0]["content"].clone();
    assert_eq!(content.as_array().unwrap().len(), 1);
    assert_eq!(
        content[0]["cache_control"],
        serde_json::json!({"type": "ephemeral"})
    );
}

#[test]
fn a_breakpoint_is_never_doubled_onto_a_block_that_has_one() {
    let body = body_with(serde_json::json!([{
        "role": "assistant",
        "content": [
            {
                "type": "thinking",
                "thinking": "half",
                "cache_control": {"type": "ephemeral"},
            },
            {
                "type": "text",
                "text": "tail",
                "cache_control": {"type": "ephemeral", "ttl": "1h"},
            },
        ],
    }]));
    let out = drop_unsigned_reasoning_blocks(body, "r");
    let content = messages_of(&out)[0]["content"].clone();
    assert_eq!(
        content[0]["cache_control"],
        serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
        "the block's own marker wins; breakpoints are a budget of four"
    );
}

#[test]
fn a_message_is_left_alone_when_dropping_would_empty_it() {
    // Upstream refuses empty content as firmly as it refuses the unsigned
    // block, so there is nothing to gain by trading one for the other.
    let body = body_with(serde_json::json!([{
        "role": "assistant",
        "content": [{"type": "thinking", "thinking": "half"}],
    }]));
    assert_eq!(drop_unsigned_reasoning_blocks(body.clone(), "r"), body);
}

#[test]
fn dropping_is_idempotent_so_the_prefix_holds_across_turns() {
    // The property the cache depends on: once a truncated turn is in the
    // history, every later turn carries it, and each must produce the same
    // bytes upstream or the prefix moves under the cache every turn.
    let body = body_with(serde_json::json!([
        {"role": "user", "content": "q"},
        {
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "half"},
                {"type": "text", "text": "[truncated: ...]"},
            ],
        },
        {"role": "user", "content": "carry on"},
    ]));
    let once = drop_unsigned_reasoning_blocks(body, "r");
    let twice = drop_unsigned_reasoning_blocks(once.clone(), "r");
    assert_eq!(once, twice, "a second pass must change nothing");
}

#[test]
fn the_tampering_guard_does_not_see_an_unsigned_drop_as_a_rewrite() {
    // `restore_client_reasoning_blocks` reverts the whole message array
    // when the outbound signed blocks stop matching the client's. If it
    // counted unsigned ones it would revert this drop every turn — and
    // with it every byte of compression on that turn.
    let client: Vec<serde_json::Value> = serde_json::from_value(serde_json::json!([{
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "real", "signature": "sig"},
            {"type": "thinking", "thinking": "half"},
            {"type": "text", "text": "tail"},
        ],
    }]))
    .unwrap();
    let dropped =
        drop_unsigned_reasoning_blocks(body_with(serde_json::Value::Array(client.clone())), "r");
    let forwarded: Vec<serde_json::Value> = serde_json::from_value(messages_of(&dropped)).unwrap();
    assert_eq!(
        signed_reasoning_blocks(&client),
        signed_reasoning_blocks(&forwarded),
        "dropping an unsigned block must leave the signed set identical"
    );
}

#[test]
fn a_signed_block_beside_an_unsigned_one_survives_verbatim() {
    let body = body_with(serde_json::json!([{
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "real", "signature": "sig"},
            {"type": "thinking", "thinking": "half"},
            {"type": "text", "text": "tail"},
        ],
    }]));
    let out = drop_unsigned_reasoning_blocks(body, "r");
    let content = messages_of(&out)[0]["content"].clone();
    assert_eq!(content.as_array().unwrap().len(), 2);
    assert_eq!(content[0]["signature"], "sig");
    assert_eq!(content[0]["thinking"], "real");
}

// ── drop_headroom_signed_reasoning_blocks ────────────────────
//
// The cost-aware router can send one turn to a routed model and the next
// back to Anthropic. The routed reply carries a signature only this proxy
// can read, and Anthropic refuses any signature it did not issue, so the
// envelope has to come off before the turn goes back.

/// A signature in the shape the routed stream actually writes, so the
/// test moves if the envelope format does.
fn our_signature() -> String {
    crate::handlers::reasoning_signature::encode_reasoning_signature(
        &crate::handlers::reasoning_signature::ReasoningReplay {
            id: "rs_1".to_string(),
            encrypted_content: "blob".to_string(),
        },
    )
    .expect("a well-formed replay encodes")
}

#[test]
fn only_our_own_envelope_comes_off_the_anthropic_bound_turn() {
    let body = body_with(serde_json::json!([{
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "native", "signature": "ErUBCkYIBRgCKkDzS1nT"},
            {"type": "thinking", "thinking": "routed", "signature": our_signature()},
            {"type": "text", "text": "answer"},
        ],
    }]));
    let out = drop_headroom_signed_reasoning_blocks(body, "r");
    let content = messages_of(&out)[0]["content"].clone();
    let blocks = content.as_array().unwrap();
    assert_eq!(blocks.len(), 2, "exactly one block should have gone");
    assert_eq!(blocks[0]["thinking"], "native");
    assert_eq!(blocks[0]["signature"], "ErUBCkYIBRgCKkDzS1nT");
    assert_eq!(blocks[1]["text"], "answer");
}

#[test]
fn a_turn_that_never_met_a_routed_model_is_byte_identical() {
    // The prefix gate: no envelope, no parse, no re-serialize. A body
    // that came back changed would move the cached prefix for every
    // conversation on the proxy, which is most of them.
    let body = body_with(serde_json::json!([{
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "native", "signature": "ErUBCkYIBRgCKkDzS1nT"},
            {"type": "text", "text": "answer"},
        ],
    }]));
    assert_eq!(
        drop_headroom_signed_reasoning_blocks(body.clone(), "r"),
        body
    );
}

#[test]
fn our_envelope_is_left_alone_when_dropping_would_empty_the_message() {
    // Same trade the unsigned stage refuses: an empty content array is
    // rejected just as firmly as the foreign signature.
    let body = body_with(serde_json::json!([{
        "role": "assistant",
        "content": [{"type": "thinking", "thinking": "routed", "signature": our_signature()}],
    }]));
    assert_eq!(
        drop_headroom_signed_reasoning_blocks(body.clone(), "r"),
        body
    );
}

#[test]
fn dropping_our_envelope_carries_its_cache_breakpoint_forward() {
    let body = body_with(serde_json::json!([{
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "routed", "signature": our_signature(),
             "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "answer"},
        ],
    }]));
    let out = drop_headroom_signed_reasoning_blocks(body, "r");
    let content = messages_of(&out)[0]["content"].clone();
    assert_eq!(content.as_array().unwrap().len(), 1);
    assert_eq!(
        content[0]["cache_control"]["type"], "ephemeral",
        "the breakpoint must ride to the surviving block, not vanish"
    );
}

#[test]
fn the_tampering_guard_does_not_see_our_envelope_as_a_rewrite() {
    // `restore_client_reasoning_blocks` reverts the whole message array
    // when the outbound signed set stops matching the client's. Counting
    // our own envelope there would put the refused block straight back.
    let client: Vec<serde_json::Value> = serde_json::from_value(serde_json::json!([{
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "native", "signature": "ErUBCkYIBRgCKkDzS1nT"},
            {"type": "thinking", "thinking": "routed", "signature": our_signature()},
            {"type": "text", "text": "tail"},
        ],
    }]))
    .unwrap();
    let dropped = drop_headroom_signed_reasoning_blocks(
        body_with(serde_json::Value::Array(client.clone())),
        "r",
    );
    let forwarded: Vec<serde_json::Value> = serde_json::from_value(messages_of(&dropped)).unwrap();
    assert_eq!(
        signed_reasoning_blocks(&client),
        signed_reasoning_blocks(&forwarded),
        "dropping our own envelope must leave the provider-signed set identical"
    );
}

#[test]
fn apply_prefix_replay_pipes_inbound_tail_evidence_to_usage_observer() {
    use crate::cache_stabilization::prefix_replay::SessionReplayStore;
    use crate::cache_stabilization::usage_observer::{RecacheEventKind, UsageObserver};

    let store = SessionReplayStore::new(2);
    let observer = UsageObserver::new();
    let session_key = "tail-evidence-session";
    let prior = vec![
        serde_json::json!({"role":"user","content":"open"}),
        serde_json::json!({"role":"assistant","content":"answer"}),
        serde_json::json!({"role":"user","content":"old tail"}),
    ];

    observer.begin_request("tail-1", "tail-conversation".into(), None, None, None);
    let prior_body = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({"messages": prior.clone()})).unwrap(),
    );
    apply_prefix_replay(
        &store,
        session_key,
        "tail-1",
        prior.clone(),
        prior_body,
        Some(&observer),
        1,
        2,
        false,
    );
    store.complete("tail-1", 0, 50_000);
    observer.complete("tail-1", 200, 0, 50_000, None);

    let mut current = prior;
    current[2] = serde_json::json!({"role":"user","content":"replacement tail"});
    observer.begin_request("tail-2", "tail-conversation".into(), None, None, None);
    let current_body = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({"messages": current.clone()})).unwrap(),
    );
    apply_prefix_replay(
        &store,
        session_key,
        "tail-2",
        current,
        current_body,
        Some(&observer),
        2,
        2,
        false,
    );
    let class = observer.complete("tail-2", 200, 0, 50_000, None);

    assert_eq!(class, None, "a branch cache build is not a cache miss");
    let snapshot = observer.snapshot();
    assert_eq!(snapshot.recache_wasted_tokens_total, 0);
    let event = snapshot.last_event.expect("branch cache build recorded");
    assert_eq!(event.event_kind, RecacheEventKind::Branch);
    assert_eq!(
        event.attribution_reason.as_deref(),
        Some("inbound_tail_replaced")
    );
    assert_eq!(event.origin.as_deref(), Some("inbound"));
    assert_eq!(event.scope.as_deref(), Some("final_message"));
}
