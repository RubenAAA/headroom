//! Serve the proxy on a Unix socket file and hit a route through it.
//!
//! Port of the transport half of upstream `tests/test_proxy_uds.py`
//! (commit `e7b3baf2`): the socket is not just prepared, it serves the
//! real app and answers a request.

use std::time::Duration;

use headroom_proxy::uds::{prepare_uds_path, remove_uds_path, socket_usage_lines, uds_router};
use headroom_proxy::{AppState, Config};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use url::Url;

async fn connect_retry(sock: &std::path::Path) -> tokio::net::UnixStream {
    let mut last_err = String::new();
    for _ in 0..50 {
        match tokio::net::UnixStream::connect(sock).await {
            Ok(stream) => return stream,
            Err(e) => {
                last_err = e.to_string();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    panic!("could not connect to {sock:?}: {last_err}");
}

#[tokio::test]
async fn proxy_serves_healthz_over_unix_socket() {
    let dir = tempfile::tempdir().unwrap();
    let prepared = prepare_uds_path(&dir.path().join("proxy.sock"))
        .await
        .unwrap();

    let state = AppState::new(Config::for_test(Url::parse("http://127.0.0.1:1").unwrap()))
        .expect("app state");
    let app = uds_router(state).into_make_service();
    let listener = tokio::net::UnixListener::bind(&prepared).unwrap();

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });

    let mut stream = connect_retry(&prepared).await;
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut body = Vec::new();
    stream.read_to_end(&mut body).await.unwrap();
    let response = String::from_utf8_lossy(&body);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "unexpected status: {response}"
    );
    assert!(
        response.contains(r#""ok":true"#),
        "unexpected body: {response}"
    );

    let _ = shutdown_tx.send(());
    server.await.unwrap().unwrap();

    // Banner lines describe the socket without naming any agent or handing
    // out configuration (upstream keeps that detail on the docs page).
    let lines = socket_usage_lines(&prepared);
    assert_eq!(lines.len(), 4);
    assert!(lines[0].contains(&prepared.to_string_lossy().into_owned()));
    assert!(lines[2].contains("--unix-socket"));
    let blob = lines.join("\n").to_lowercase();
    assert!(!blob.contains("anthropic"), "banner names no agent: {blob}");

    remove_uds_path(&prepared);
    assert!(!prepared.exists(), "socket file removed on exit");
}
