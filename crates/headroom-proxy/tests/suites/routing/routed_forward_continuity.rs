//! `routed_forward_continuity` names where a routed turn's forwarded input
//! first differs from the session's previous turn.
//!
//! That is the number that explains a provider cache miss: an appended turn
//! keeps every earlier item as prefix, an edited earlier item breaks it at that
//! item. The test drives three turns through a routed model and reads the
//! events back.

use super::common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use common::start_proxy_with_state;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

async fn upstream() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<hyper::body::Incoming>| async move {
                    let _ = req.into_body().collect().await;
                    let payload = [
                        sse(
                            "response.created",
                            json!({"type":"response.created","response":{"id":"resp_1"}}),
                        ),
                        sse(
                            "response.output_text.done",
                            json!({"type":"response.output_text.done",
                                   "item_id":"msg_1","output_index":0,"text":"ok"}),
                        ),
                        sse(
                            "response.completed",
                            json!({"type":"response.completed","response":
                                   {"id":"resp_1","status":"completed",
                                    "usage":{"input_tokens":10,"output_tokens":1}}}),
                        ),
                    ]
                    .concat();
                    let stream = futures_util::stream::iter(vec![Ok::<_, Infallible>(
                        Frame::data(Bytes::from(payload)),
                    )]);
                    Ok::<_, Infallible>(
                        Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(StreamBody::new(stream))
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, task)
}

fn turn(texts: &[&str]) -> Value {
    let messages: Vec<Value> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"role": if i % 2 == 0 {"user"} else {"assistant"}, "content": t}))
        .collect();
    json!({"model": "claude-spark-test", "stream": true, "messages": messages})
}

#[tokio::test]
async fn an_appended_turn_keeps_its_prefix_and_an_edited_one_breaks_it() {
    let _capture = common::tracing_capture::serial().await;
    let buf = common::tracing_capture::buffer();
    buf.lock().unwrap().clear();

    let (addr, _task) = upstream().await;
    let upstream_url = format!("http://{addr}");
    let route_upstream = upstream_url.clone();
    let proxy = start_proxy_with_state(
        &upstream_url,
        move |config| {
            config.model_routes = vec![headroom_proxy::config::ProviderRoute {
                model_prefix: "claude-spark-test".to_string(),
                prefix_match: false,
                upstream: Some(route_upstream.parse().expect("upstream url")),
                translate: true,
                cursor_agent: None,
                target_model: Some("spark-test-model".to_string()),
                auth_env: Some("none".to_string()),
            }];
        },
        |state| state,
    )
    .await;

    let client = reqwest::Client::new();
    for texts in [
        vec!["task", "a1", "u2"],
        vec!["task", "a1", "u2", "a2", "u3"],
        vec!["task", "a1", "EDITED", "a2", "u3", "a3", "u4"],
    ] {
        client
            .post(format!("{}/v1/messages", proxy.url()))
            .header("content-type", "application/json")
            .json(&turn(&texts))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .expect("proxy answers")
            .text()
            .await
            .unwrap();
    }

    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    let events: Vec<Value> = logs
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| v["fields"].clone())
        .filter(|f| f["event"] == "routed_forward_continuity")
        .collect();
    assert_eq!(events.len(), 3, "one event per routed turn: {events:?}");

    assert_eq!(events[0]["first_turn"], true);
    // Append: every previous item is still there, nothing broke.
    assert_eq!(events[1]["prefix_broken"], false, "{}", events[1]);
    assert_eq!(events[1]["common_prefix"], events[1]["prev_items"]);
    // Edit at message 2: the prefix ends at the item before it.
    assert_eq!(events[2]["prefix_broken"], true, "{}", events[2]);
    assert!(
        events[2]["common_prefix"].as_u64().unwrap() < events[2]["prev_items"].as_u64().unwrap()
    );
    assert_eq!(events[2]["head_changed"], false);

    proxy.shutdown().await;
}
