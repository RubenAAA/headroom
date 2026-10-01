//! `headroom classify` end to end: the real binary against a wiremock Zen.
//!
//! Each case asserts what reached the endpoint and what the user sees. A
//! missing state or empty questions must stop the CLI before any request,
//! because Zen answers both with a 422 that reads "Endpoint is unavailable".

use std::process::{Command, Output};

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ENDPOINT_PATH: &str = "/zen/v1/systemone";

fn reply() -> Value {
    json!({
        "model": "jev-1.13-free",
        "answers": {
            "q1": {"type": "noul", "noul": 0.96},
            "team": {"type": "choice", "choice": "billing", "confidence": 1,
                     "probabilities": {"billing": 1, "shipping": 0}},
            "anger": {"type": "score", "score": 1.1, "confidence": 0.82,
                      "legend": {"0": "Calm", "1": "Frustrated"},
                      "probabilities": {"0": 0.01, "1": 0.88}}
        },
        "usage": {"input_tokens": 368, "output_tokens": 57},
        "cost": "0"
    })
}

async fn zen(status: u16, body: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ENDPOINT_PATH))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    server
}

/// Run the CLI off the async runtime so the mock server keeps serving.
async fn run(server: &MockServer, args: &[&str], key: Option<&str>) -> Output {
    let url = format!("{}{ENDPOINT_PATH}", server.uri());
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let key = key.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_headroom"));
        cmd.arg("classify")
            .args(&args)
            .env("HEADROOM_JEV_URL", url)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env_remove("OPENCODE_API_KEY");
        if let Some(k) = key {
            cmd.env("OPENCODE_API_KEY", k);
        }
        cmd.output().expect("spawn headroom")
    })
    .await
    .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_state_and_empty_questions_fail_before_any_request() {
    let server = zen(200, reply()).await;

    let no_noul = run(&server, &["--state", "hello"], None).await;
    assert!(!no_noul.status.success());
    assert!(
        err(&no_noul).contains("at least one --noul"),
        "{}",
        err(&no_noul)
    );

    let blank = run(&server, &["--state", "  ", "--noul", "Is it urgent?"], None).await;
    assert!(!blank.status.success());
    assert!(err(&blank).contains("state is empty"), "{}", err(&blank));

    let req = r#"{"state":"hello","questions":{}}"#;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("req.json");
    std::fs::write(&file, req).unwrap();
    let empty_q = run(&server, &["--request", file.to_str().unwrap()], None).await;
    assert!(!empty_q.status.success());
    assert!(
        err(&empty_q).contains("non-empty object"),
        "{}",
        err(&empty_q)
    );

    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "a request reached Zen"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noul_flags_become_q1_q2_and_the_free_model_sends_no_key() {
    let server = zen(200, reply()).await;
    let o = run(
        &server,
        &[
            "--state",
            "Payments failed for three days.",
            "--noul",
            "Is it urgent?",
            "--noul",
            "Is it billing?",
        ],
        Some("a-real-looking-key"),
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(
        sent,
        json!({
            "model": "jev-1.13-free",
            "state": "Payments failed for three days.",
            "questions": {
                "q1": {"type": "noul", "instructions": "Is it urgent?"},
                "q2": {"type": "noul", "instructions": "Is it billing?"}
            }
        })
    );
    assert!(
        reqs[0].headers.get("authorization").is_none(),
        "the free model got a key"
    );
    let ua = reqs[0].headers.get("user-agent").unwrap().to_str().unwrap();
    assert!(ua.starts_with("headroom-cli/"), "user-agent was {ua}");

    // One line per answer, in the order the model returned them.
    assert_eq!(
        out(&o).trim(),
        "q1: 0.96\nteam: billing (confidence 1.00)\nanger: 1.10 (confidence 0.82)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answers_print_in_the_order_asked_not_the_order_returned() {
    let server = zen(
        200,
        json!({"answers": {
            "q2": {"type": "noul", "noul": 0.07},
            "q1": {"type": "noul", "noul": 0.96}
        }}),
    )
    .await;
    let o = run(
        &server,
        &["--state", "x", "--noul", "a", "--noul", "b"],
        None,
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(out(&o).trim(), "q1: 0.96\nq2: 0.07");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_file_carries_choice_and_a_paid_model_sends_the_key() {
    let server = zen(200, reply()).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("req.json");
    let body = json!({
        "model": "jev-1.13",
        "state": "Refund please.",
        "questions": {"team": {"type": "choice", "instructions": "Who?",
                               "criteria": {"billing": null, "shipping": null}}}
    });
    std::fs::write(&file, body.to_string()).unwrap();

    let o = run(
        &server,
        &["--request", file.to_str().unwrap(), "--json"],
        Some("sk-test"),
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));

    let reqs = server.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(sent, body, "the request file went out unchanged");
    assert_eq!(
        reqs[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer sk-test"
    );
    let printed: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(printed["answers"]["team"]["choice"], "billing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_errors_exit_nonzero_with_status_and_body() {
    let server = zen(
        400,
        json!({"detail": "Too many choices. Must have at most 255 choices."}),
    )
    .await;
    let o = run(&server, &["--state", "x", "--noul", "y"], None).await;
    assert!(!o.status.success());
    let e = err(&o);
    assert!(e.contains("HTTP 400"), "{e}");
    assert!(e.contains("Too many choices"), "{e}");
    assert!(out(&o).is_empty());
}
