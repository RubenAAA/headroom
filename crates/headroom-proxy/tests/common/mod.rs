//! Shared test harness: spin up a Rust proxy bound to an ephemeral port
//! pointed at an arbitrary upstream URL.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use headroom_proxy::vertex::TokenSource;
use headroom_proxy::{build_app, AppState, Config};
use tokio::sync::oneshot;
use url::Url;

#[allow(dead_code)]
pub struct ProxyHandle {
    pub addr: SocketAddr,
    pub shutdown: Option<oneshot::Sender<()>>,
    pub task: tokio::task::JoinHandle<()>,
}

#[allow(dead_code)]
impl ProxyHandle {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub fn ws_url(&self) -> String {
        format!("ws://{}", self.addr)
    }
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.task.await;
    }
}

#[allow(dead_code)]
pub async fn start_proxy(upstream: &str) -> ProxyHandle {
    start_proxy_with(upstream, |_| {}).await
}

/// Start a proxy with a customized `Config`. The closure receives a
/// mutable reference to the default `Config::for_test` and may toggle
/// flags like `compression` before the proxy is built.
#[allow(dead_code)]
pub async fn start_proxy_with<F>(upstream: &str, customize: F) -> ProxyHandle
where
    F: FnOnce(&mut Config),
{
    start_proxy_with_state(upstream, customize, |s| s).await
}

/// Start a proxy with both a Config customizer and an AppState
/// post-processor. PR-D1: tests that exercise the Bedrock route
/// inject credentials via `with_bedrock_credentials` here.
/// PR-D4: Vertex tests inject a `StaticTokenSource` via
/// `install_static_token_source` here (chain-style) so they never
/// hit real GCP.
#[allow(dead_code)]
pub async fn start_proxy_with_state<F, G>(
    upstream: &str,
    customize: F,
    customize_state: G,
) -> ProxyHandle
where
    F: FnOnce(&mut Config),
    G: FnOnce(AppState) -> AppState,
{
    let upstream_url: Url = upstream.parse().expect("valid upstream url");
    let mut config = Config::for_test(upstream_url);
    customize(&mut config);
    let state = AppState::new(config.clone()).expect("app state");
    let state = customize_state(state);
    let app = build_app(state).into_make_service_with_connect_info::<SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    // Wait for the listener to accept instead of a fixed sleep: typically
    // ready in ~1ms, with a 2s ceiling for slow CI (was: sleep 20ms per
    // proxy boot × ~300 tests).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(stream) => {
                drop(stream);
                break;
            }
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(e) => panic!("proxy listener on {addr} never became ready: {e}"),
        }
    }
    ProxyHandle {
        addr,
        shutdown: Some(tx),
        task,
    }
}

/// Convenience: replace the default `vertex_token_source` with a
/// `StaticTokenSource` returning the supplied bearer string. Used by
/// the PR-D4 Vertex integration tests so they never hit real GCP.
///
/// PR-D4: chain-style helper to install a `StaticTokenSource` on an
/// `AppState`. Returns the modified state so it composes with
/// `start_proxy_with_state`'s `FnOnce(AppState) -> AppState`.
#[allow(dead_code)]
pub fn install_static_token_source(mut state: AppState, bearer: &str) -> AppState {
    state.vertex_token_source = Arc::new(headroom_proxy::vertex::StaticTokenSource::new(
        bearer.to_string(),
    )) as Arc<dyn TokenSource>;
    state
}

/// Hold a reference to the config so dead_code doesn't strip its use.
#[allow(dead_code)]
pub fn _config_ref() -> Arc<Config> {
    Arc::new(Config::for_test(Url::parse("http://127.0.0.1:1").unwrap()))
}

// ─── Shared fixtures: one client, event-driven waits ────────────────────
//
// Every integration binary used to build a fresh `reqwest::Client` per test
// (TLS + connection-pool init each time) and pace overlap handoffs with
// fixed `sleep(200ms)`s. The helpers below replace both: a process-wide
// client and polls that return as soon as the observed condition holds.

/// Process-wide HTTP client. `Client::clone` is cheap (Arc-interned pool);
/// `Client::new` is not — share this instead of constructing per test.
#[allow(dead_code)]
pub fn shared_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new).clone()
}

/// Poll `cond` every `interval` until it returns true or `timeout` elapses.
/// Returns true when the condition held in time. Event-driven tests should
/// prefer this over a fixed `sleep`: fast when the system is fast, still
/// robust on slow CI.
#[allow(dead_code)]
pub async fn wait_until<F>(timeout: Duration, interval: Duration, mut cond: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(interval).await;
    }
    cond()
}

/// Wait until the mock upstream has seen at least `n` requests (2s ceiling).
/// Use after spawning a request task instead of `sleep(200ms)`: it proves
/// the request is actually in flight rather than assuming 200ms sufficed.
#[allow(dead_code)]
pub async fn wait_for_upstream_requests(
    upstream: &wiremock::MockServer,
    n: usize,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let seen = upstream
            .received_requests()
            .await
            .map(|r| r.len())
            .unwrap_or(0);
        if seen >= n {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "upstream saw {seen}/{n} requests before the {timeout:?} deadline"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
