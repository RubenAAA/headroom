//! headroom-proxy: transparent reverse proxy binary.
//!
//! Drops in front of the existing Python proxy. End-users hit the public
//! port; this binary forwards every HTTP/SSE/WebSocket request verbatim to
//! `--upstream`. See RUST_DEV.md for the operator runbook.

use std::net::SocketAddr;
use std::time::UNIX_EPOCH;

use clap::Parser;
use headroom_proxy::config::CliArgs;
use headroom_proxy::{build_app, AppState, Config};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = CliArgs::parse();
    let config = Config::from_cli(args);

    init_tracing(&config.log_level);
    headroom_core::init_ort_ep();

    // Runs after tracing is up so the warning is actually visible.
    headroom_proxy::config::warn_on_ambiguous_codex_routes(
        &config.model_routes,
        config.codex_auth_file.as_deref(),
    );
    headroom_proxy::config::warn_on_shadowed_routes(&config.model_routes);

    // Before the first turn, because that is the only time it can be acted on:
    // a NIC left with receive offload on corrupts TLS records for the whole
    // session, and the turns it kills are paid for.
    headroom_proxy::net_offload::warn_if_offload_corrupts_tls();

    // Identity, so a log line can be tied to the process and the build that
    // wrote it. The log outlives any one run — it is appended across restarts
    // and reboots — and carried exactly one `headroom-proxy starting` marker
    // across five files and 22 runs, so "scope the measurement by process
    // start", which is the rule every re-cache number here depends on, could
    // not actually be followed. Version alone does not separate two builds of
    // the same version; the binary's size and mtime do.
    let (binary_len, binary_mtime) = std::env::current_exe()
        .and_then(|path| path.metadata())
        .map(|meta| {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            (meta.len(), mtime)
        })
        .unwrap_or((0, 0));

    tracing::info!(
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        binary_len = binary_len,
        binary_mtime = binary_mtime,
        listen = %config.listen,
        upstream = %config.upstream,
        upstream_timeout_s = config.upstream_timeout.as_secs(),
        upstream_connect_timeout_s = config.upstream_connect_timeout.as_secs(),
        max_body_bytes = config.max_body_bytes,
        rewrite_host = config.rewrite_host,
        graceful_shutdown_timeout_s = config.graceful_shutdown_timeout.as_secs(),
        rollout_channel = config.rollout.config.channel.as_str(),
        rollout_features_enabled = ?config.rollout.enabled(),
        rollout_features_disabled = ?config.rollout.config.disabled,
        unsafe_allow_unstable_features = config.rollout.config.unsafe_allow_unstable,
        rollout_registry_digest = %config.rollout.registry_digest,
        rollout_snapshot_digest = %config.rollout.snapshot_digest(),
        qualification_eligible = config.rollout.qualification_eligible(),
        "headroom-proxy starting"
    );

    // Session-sticky beta headers only run inside the compression
    // interceptor: with `--compression` off the proxy is a strict
    // byte-pipe and never mutates headers. Say so loudly at startup —
    // an operator reading `beta_header_sticky=enabled` (the default)
    // must not believe the protection is active when it isn't.
    if config.beta_header_sticky.is_enabled() && !config.compression {
        tracing::warn!(
            event = "beta_header_sticky_inactive",
            beta_header_sticky = config.beta_header_sticky.as_str(),
            compression = config.compression,
            "beta-header stickiness is enabled but the compression \
             interceptor is off; enable --compression (or \
             HEADROOM_PROXY_COMPRESSION=1) to activate it"
        );
    }

    // Licensing lives only in the Python proxy: it reads HEADROOM_LICENSE_KEY,
    // builds a `UsageReporter`, and lets an expired key past its grace period
    // turn compression off. This binary has no license plumbing at all, so it
    // compresses regardless — which is right for every deployment without a
    // key, and wrong in silence for one with a key that expects enforcement
    // and usage reporting. Now that the Rust proxy is the default dispatch,
    // say so rather than letting an operator infer from the Python banner
    // that a managed deployment is still being metered.
    if std::env::var("HEADROOM_LICENSE_KEY").is_ok_and(|k| !k.trim().is_empty()) {
        tracing::warn!(
            event = "license_key_ignored",
            "HEADROOM_LICENSE_KEY is set but this proxy has no licensing: \
             no usage is reported and no expiry is enforced; run the Python \
             proxy (HEADROOM_USE_PYTHON_PROXY=1) if a managed deployment \
             needs either"
        );
    }

    let mut state = AppState::new(config.clone())?;

    // PR-D1: resolve AWS credentials at startup via the `aws-config`
    // default chain. Loaded once so per-request signing is cheap.
    // Failure is NOT fatal — the proxy may run in front of a non-AWS
    // upstream — but the Bedrock invoke handler refuses to forward
    // unsigned requests when `bedrock_credentials` is `None`
    // (see `bedrock::invoke::handle_invoke`).
    if config.enable_bedrock_native {
        match load_bedrock_credentials(&config).await {
            Ok(creds) => {
                state = state.with_bedrock_credentials(creds);
                tracing::info!(
                    event = "bedrock_credentials_loaded",
                    region = %config.bedrock_region,
                    profile = ?config.aws_profile,
                    "AWS credentials resolved for Bedrock SigV4 signing"
                );
            }
            Err(e) => {
                tracing::warn!(
                    event = "bedrock_credentials_unavailable",
                    region = %config.bedrock_region,
                    profile = ?config.aws_profile,
                    error = %e,
                    "AWS credentials not available at startup; Bedrock invoke will 5xx until creds are configured"
                );
            }
        }
    }

    // Gate + (when enabled) eagerly warm the Kompress PlainText compressor.
    // Default-off: it carries a ~261 MB cache-only model, so it loads only
    // when an operator opts in via `--enable-kompress`. The warm runs on a
    // blocking thread off the request path; cache-only means a cold cache
    // just leaves it deferred (PlainText passes through) rather than stalling
    // startup. The always-on structural compressors + CodeCompressor need no
    // such gate.
    headroom_core::transforms::set_kompress_enabled(config.enable_kompress);
    if config.enable_kompress {
        tokio::task::spawn_blocking(|| {
            let ready = headroom_core::transforms::warm_live_zone_compressors();
            tracing::info!(
                event = "kompress_warm",
                kompress_ready = ready,
                "Kompress enabled; warmed live-zone compressors (cache-only)"
            );
        });
    }

    // cc-switch reconciler (opt-in: HEADROOM_CC_SWITCH_RECONCILE=1).
    // Watches ~/.claude/settings.json and keeps Headroom in the request
    // path when cc-switch overwrites ANTHROPIC_BASE_URL on provider switch.
    let _cc_reconciler = if headroom_proxy::cc_switch_reconciler::reconciler_enabled() {
        let reconciler = headroom_proxy::cc_switch_reconciler::CCSwitchReconciler::new(
            format!("http://{}", config.listen),
            config.upstream.to_string(),
            state.dynamic_upstream.clone(),
            headroom_proxy::cc_switch_reconciler::route_official(),
            None,
        );
        reconciler.start();
        Some(reconciler)
    } else {
        None
    };

    spawn_resource_heartbeat();

    // Reap Cursor conversations abandoned mid-tool. Each one is a live
    // `cursor-agent` process blocked on a tool result that is not coming, and
    // parking a new one sweeps the old, so this timer only matters on a proxy
    // that has gone quiet — which is exactly when a forgotten process would sit
    // longest.
    //
    // Not spawned at all without a `cursor:` route. The sweep would be a no-op
    // over an empty map, but a task that cannot do anything is a task that
    // still has to be explained to whoever reads the process next.
    if config
        .model_routes
        .iter()
        .any(|route| route.cursor_agent.is_some())
    {
        let bridge = state.cursor_bridge.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                let reaped = bridge
                    .reap_idle(headroom_proxy::cursor::bridge::MAX_PARK)
                    .await;
                if reaped > 0 {
                    tracing::info!(
                        event = "cursor_reaper_swept",
                        reaped,
                        "reaped abandoned cursor conversations"
                    );
                }
            }
        });
    }

    // Binding a non-loopback interface with no token leaves every `/v1/*`
    // route reachable from the surrounding network — the shape the 0.0.0.0
    // container image has by default. Say so at boot, where an operator will
    // see it, rather than leaving it to be discovered.
    if config.proxy_token.is_none()
        && !headroom_proxy::loopback_guard::is_loopback_host(Some(
            &config.listen.ip().to_string(),
        ))
    {
        tracing::warn!(
            event = "proxy_open_bind",
            host = %config.listen.ip(),
            "bound to a non-loopback interface with no HEADROOM_PROXY_TOKEN; \
             the /v1/* routes are reachable WITHOUT authentication"
        );
    }

    let app = build_app(state).into_make_service_with_connect_info::<SocketAddr>();

    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");

    let grace = config.graceful_shutdown_timeout;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!(
                timeout_s = grace.as_secs(),
                "draining in-flight requests before exit"
            );
            tokio::time::sleep(grace).await;
        })
        .await?;

    Ok(())
}

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    let json_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(false)
        .with_span_list(false);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(json_layer)
        .try_init();
}

/// PR-D1: resolve AWS credentials for Bedrock SigV4 signing.
///
/// Uses the `aws-config` default chain (env vars → shared profile
/// file → IMDS / ECS task role). Honours `Config::aws_profile` when
/// set; otherwise the chain picks up `AWS_PROFILE` from the
/// environment automatically.
async fn load_bedrock_credentials(
    config: &Config,
) -> Result<aws_credential_types::Credentials, Box<dyn std::error::Error + Send + Sync>> {
    use aws_config::BehaviorVersion;
    use aws_credential_types::provider::ProvideCredentials;

    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_config::Region::new(config.bedrock_region.clone()));
    if let Some(profile) = config.aws_profile.as_deref() {
        loader = loader.profile_name(profile);
    }
    let aws_config = loader.load().await;
    let creds_provider = aws_config
        .credentials_provider()
        .ok_or("no credentials provider configured")?;
    let creds = creds_provider.provide_credentials().await?;
    Ok(creds)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

/// Log the process's own memory and thread count once a minute.
///
/// The proxy reported plenty about the traffic it shaped and nothing about
/// what it cost to run, so a leak or a thread pile-up could only be caught by
/// watching from outside with `ps`. One line a minute is cheap and gives the
/// growth a shape after the fact.
///
/// Reads `/proc/self/status`, so it is a no-op on platforms without it.
fn spawn_resource_heartbeat() {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        // The first tick fires at once; skip it so the reading is not taken
        // before the server has finished coming up.
        tick.tick().await;
        loop {
            tick.tick().await;
            let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
                return;
            };
            let field = |name: &str| -> u64 {
                status
                    .lines()
                    .find(|l| l.starts_with(name))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            };
            let open_fds = std::fs::read_dir("/proc/self/fd")
                .map(|d| d.count() as u64)
                .unwrap_or(0);
            tracing::info!(
                event = "resource_heartbeat",
                rss_mb = field("VmRSS:") / 1024,
                peak_rss_mb = field("VmHWM:") / 1024,
                threads = field("Threads:"),
                open_fds,
            );
        }
    });
}
