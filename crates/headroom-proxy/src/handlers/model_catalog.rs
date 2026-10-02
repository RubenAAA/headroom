//! The routed models a client can pick, what each needs before it answers,
//! and whether this machine has it.
//!
//! Two readers. `GET /models-status` serves the list to the `/models`
//! command. The typo guard on `/v1/messages` serves it to a client that
//! asked for a name one or two edits from a routed alias
//! (`calude-muse-spark-1.3`): forwarded, Anthropic answers 404, and Claude
//! Code shows that as "model not found" with no hint of the names that do
//! exist. The guard answers 400 instead, whose message Claude Code prints
//! as written, in `/model` and on a turn alike.

use crate::config::{Config, ProviderRoute};
use crate::proxy::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::time::Duration;

/// Edits (insert, delete, swap of neighbours) a name may be from an alias
/// and still read as a typo of it. `calude` for `claude` is one swap; two
/// leaves room for a second slip without reaching a different alias, as
/// the closest pair in the shipped table (`-sol`/`-luna`) is further apart.
const MAX_TYPO_EDITS: usize = 2;

/// `agent status` answers in about 2 s; past this it is treated as unknown.
const CURSOR_STATUS_TIMEOUT: Duration = Duration::from_secs(10);

/// What a route needs from the user before it can answer.
enum Access {
    /// Anonymous upstream: nothing to sign up for.
    Free,
    /// A bearer token in the named environment variable of the proxy.
    ApiKey(String),
    /// The ChatGPT login in `--codex-auth-file`.
    ChatGpt(String),
    /// The `cursor-agent` CLI's own login.
    Cursor(String),
    /// Whatever credential the client sends; the proxy cannot see ahead.
    Client,
}

/// Same filter as gateway discovery: Claude Code lists only these.
fn discoverable(route: &ProviderRoute) -> bool {
    !route.prefix_match
        && (route.model_prefix.starts_with("claude") || route.model_prefix.starts_with("anthropic"))
}

fn access(config: &Config, route: &ProviderRoute) -> Access {
    if route.cursor_agent.is_some() {
        return Access::Cursor(config.cursor_agent_binary.clone());
    }
    match route.auth_env.as_deref() {
        Some("none") => Access::Free,
        Some(var) => Access::ApiKey(var.to_string()),
        // Mirrors `resolve_upstream_auth`: only a translated route takes the
        // Codex login; an Anthropic-shaped one passes the client's key on.
        None if route.translate => match &config.codex_auth_file {
            Some(file) => Access::ChatGpt(file.clone()),
            None => Access::Client,
        },
        None => Access::Client,
    }
}

/// `Some(true)` connected, `Some(false)` not, `None` when it cannot tell.
///
/// `cursor` keeps the first `agent status` answer: every Cursor route shares
/// the one binary, and each call costs about 1.5 s.
async fn connected(access: &Access, cursor: &mut Option<Option<bool>>) -> Option<bool> {
    match access {
        Access::Free => Some(true),
        Access::ApiKey(var) => Some(std::env::var(var).is_ok_and(|v| !v.trim().is_empty())),
        Access::ChatGpt(file) => Some(crate::codex::read_codex_access_token(file).is_some()),
        Access::Cursor(binary) => match *cursor {
            Some(known) => known,
            None => *cursor.insert(cursor_logged_in(binary).await),
        },
        Access::Client => None,
    }
}

/// Asks the CLI, not its files: where it keeps the login differs by OS.
async fn cursor_logged_in(binary: &str) -> Option<bool> {
    let run = tokio::process::Command::new(binary)
        .arg("status")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(CURSOR_STATUS_TIMEOUT, run).await {
        Ok(Ok(out)) => {
            let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
            Some(
                out.status.success()
                    && text.contains("logged in")
                    && !text.contains("not logged in"),
            )
        }
        // Not installed (or not on the proxy's PATH): no login to use.
        Ok(Err(_)) => Some(false),
        Err(_) => None,
    }
}

fn needs(access: &Access) -> String {
    match access {
        Access::Free => "free, no login".to_string(),
        Access::ApiKey(var) => format!("API key in {var}"),
        Access::ChatGpt(_) => "ChatGPT subscription (Codex login)".to_string(),
        Access::Cursor(_) => "Cursor subscription".to_string(),
        Access::Client => "your Claude Code credentials".to_string(),
    }
}

/// What to do when the route is not connected.
fn fix(access: &Access) -> Option<String> {
    match access {
        Access::ApiKey(var) => Some(format!("export {var} and restart-headroom.sh")),
        Access::ChatGpt(file) => Some(format!("run `codex login` (proxy reads {file})")),
        Access::Cursor(binary) => Some(format!("run `{binary} login`")),
        Access::Free | Access::Client => None,
    }
}

/// One row per model a client can pick, in flag-file order.
async fn catalog(config: &Config) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut cursor = None;
    for route in config.model_routes.iter().filter(|r| discoverable(r)) {
        let access = access(config, route);
        let connected = connected(&access, &mut cursor).await;
        let real = route
            .cursor_agent
            .as_ref()
            .or(route.target_model.as_ref())
            .unwrap_or(&route.model_prefix);
        rows.push(json!({
            "model": route.model_prefix,
            "upstream_model": real,
            "needs": needs(&access),
            "free": matches!(access, Access::Free),
            "connected": connected,
            "fix": if connected == Some(false) { fix(&access) } else { None },
        }));
    }
    rows
}

/// GET `/models-status`: the catalog as JSON, for the `/models` command.
pub async fn handle_models_status(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(json!({ "models": catalog(&state.config).await }))
}

/// The routed alias `model` is a typo of, when it is one.
fn near_miss<'a>(config: &'a Config, model: &str) -> Option<&'a str> {
    if model.is_empty() || config.model_routes.iter().any(|r| r.matches(model)) {
        return None;
    }
    if config.local_model.as_deref() == Some(model) {
        return None;
    }
    let wanted = model.to_lowercase();
    config
        .model_routes
        .iter()
        .filter(|r| discoverable(r))
        .map(|r| {
            let d = strsim::osa_distance(&wanted, &r.model_prefix.to_lowercase());
            (d, r.model_prefix.as_str())
        })
        .filter(|(d, _)| *d <= MAX_TYPO_EDITS)
        .min_by_key(|(d, _)| *d)
        .map(|(_, alias)| alias)
}

fn status_line(row: &Value, width: usize) -> String {
    let model = row["model"].as_str().unwrap_or_default();
    let needs = row["needs"].as_str().unwrap_or_default();
    let state = match row["connected"].as_bool() {
        _ if row["free"].as_bool() == Some(true) => String::new(),
        Some(true) => ": connected".to_string(),
        Some(false) => match row["fix"].as_str() {
            Some(fix) => format!(": not connected, {fix}"),
            None => ": not connected".to_string(),
        },
        None => String::new(),
    };
    format!("  {model:<width$}  {needs}{state}")
}

/// Answer a mistyped routed alias with the names that exist, instead of
/// sending it on to a 404. `None` lets the turn proceed.
pub(crate) async fn reject_near_miss(config: &Config, model: &str) -> Option<Response> {
    let meant = near_miss(config, model)?;
    let mut lines = vec![
        format!("Unknown model '{model}'. Did you mean '{meant}'?"),
        "Models this proxy routes:".to_string(),
    ];
    let rows = catalog(config).await;
    let width = rows
        .iter()
        .filter_map(|r| r["model"].as_str())
        .map(str::len)
        .max()
        .unwrap_or(0);
    lines.extend(rows.iter().map(|r| status_line(r, width)));
    lines.push(
        "Pick one with /model <name>; /models lists them. Anthropic models pass through as before"
            .to_string(),
    );
    tracing::info!(
        event = "model_typo_rejected",
        model = %model,
        meant = %meant,
        "rejected a mistyped routed model name"
    );
    let body = json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": lines.join("\n")},
    });
    Some((StatusCode::BAD_REQUEST, axum::Json(body)).into_response())
}
