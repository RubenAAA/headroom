//! Tell an upstream free-tier outage apart from a proxy-side regression or a
//! blocked egress address.
//!
//! Zen answers 403 `FreeTierError` ("can only be used from within OpenCode")
//! when its gate refuses a request. OpenCode may have changed the gate so it
//! refuses its own client too (2026-10-03 and 2026-10-14: stock OpenCode
//! failed the same way, and nothing on this side could help). The gate may
//! now want something the stock client sends and the proxy does not (the
//! lowercase tool names in `routed::tool_alias`, the forced stream in
//! `routed::translation`). Or it may refuse the address of the relay lane.
//! The first means wait, the second a proxy fix, the third a lane rotation.
//!
//! On a refusal the proxy runs the stock CLI anonymously against the same
//! model, through the relay lane the refused request used, and logs the
//! answer as `zen_gate_probe`. Refused there too, it runs once more from this
//! machine's own address, which tells an outage from a blocked lane address.
//! The runs use their own HOME, so they carry no key and leave no session in
//! the operator's OpenCode database.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Minimum gap between probes. An outage refuses every request for hours;
/// one answer per window is enough, and each probe is a model call.
const PROBE_MIN_INTERVAL: Duration = Duration::from_secs(900);

/// Bound on one CLI run. A first run in a fresh HOME fetches the model
/// catalog before its call (about 20 s measured 2026-10-15).
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// The text of Zen's refusal, as the server sends it and as the CLI prints it.
const REFUSAL_TEXT: &str = "free tier can only be used";

/// Probe guard state: `(in flight, last attempt)`.
static PROBE_STATE: std::sync::OnceLock<Mutex<(bool, Option<Instant>)>> =
    std::sync::OnceLock::new();

/// HTTP proxy URL of each Zen egress slot, indexed like the egress pool;
/// `None` for a lane the CLI cannot use.
static LANES: Mutex<Vec<Option<String>>> = Mutex::new(Vec::new());

/// Record the Zen egress pool, in slot order, so a probe can leave through
/// the lane its refused request used.
pub(crate) fn set_lanes(pool: &[&String]) {
    if let Ok(mut lanes) = LANES.lock() {
        *lanes = pool.iter().map(|url| http_lane(url)).collect();
    }
}

/// The relay's lanes are `socks5h://127.0.0.1:PORT` and also take HTTP
/// `CONNECT` on that port. The CLI runs on Bun, whose fetch refuses SOCKS
/// proxies, so only the HTTP form works. Any other proxy, remote or with
/// credentials, cannot be reached that way.
fn http_lane(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let port = parsed.port()?;
    let relay = matches!(parsed.scheme(), "socks5" | "socks5h")
        && parsed.host_str() == Some("127.0.0.1")
        && parsed.username().is_empty()
        && parsed.password().is_none();
    relay.then(|| format!("http://127.0.0.1:{port}"))
}

/// Whether an upstream error is Zen's free-tier refusal.
pub(crate) fn is_free_tier_refusal(status: http::StatusCode, body: &str) -> bool {
    status == http::StatusCode::FORBIDDEN && body.contains("FreeTierError")
}

/// Start a background probe for `model` (the upstream id, e.g.
/// `muse-spark-1.3-contributor-free`) refused on `egress_slot`, unless one is
/// running or the last one is too recent. Never blocks the request.
pub(crate) fn trigger(model: &str, egress_slot: Option<usize>) {
    // Unit tests reach the error arm with Zen-shaped bodies; a developer
    // machine has the real CLI, and a test must not spend a model call.
    if cfg!(test) {
        return;
    }
    let Some(bin) = crate::routed::quirks::resolve_opencode_bin() else {
        tracing::warn!(
            event = "zen_gate_probe",
            verdict = "skipped",
            model = %model,
            "Zen refused the free tier; no opencode CLI to check whether it refuses OpenCode too"
        );
        return;
    };
    if !claim() {
        return;
    }
    let lane = egress_slot.and_then(|slot| LANES.lock().ok()?.get(slot).cloned().flatten());
    let model = model.to_string();
    std::thread::spawn(move || {
        let through_lane = lane
            .as_deref()
            .map(|url| run_settled(&bin, &model, Some(url)));
        let verdict = decide(through_lane, || run_settled(&bin, &model, None));
        log(&model, egress_slot, lane.is_some(), &verdict);
        release();
    });
}

/// What one stock CLI run got from Zen.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Refused,
    Served,
    /// Neither, with the reason.
    Unclear(String),
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Stock OpenCode is refused from this machine too: OpenCode's side,
    /// wait it out.
    Upstream,
    /// Stock OpenCode is served through the same lane: the gate wants
    /// something the proxy no longer sends.
    Proxy,
    /// Refused through the lane, served from this machine: Zen blocks the
    /// lane's exit address.
    Lane,
    /// Served from this machine, but the lane could not be tried: either
    /// `Proxy` or `Lane`.
    ProxyOrLane,
    Inconclusive(String),
}

/// Combine the run through the lane (`None` when there is no usable lane)
/// with a run from this machine, made only when the lane run was not served.
fn decide(through_lane: Option<Outcome>, direct: impl FnOnce() -> Outcome) -> Verdict {
    if through_lane == Some(Outcome::Served) {
        return Verdict::Proxy;
    }
    match (through_lane, direct()) {
        (Some(Outcome::Refused), Outcome::Served) => Verdict::Lane,
        (_, Outcome::Served) => Verdict::ProxyOrLane,
        (_, Outcome::Refused) => Verdict::Upstream,
        (_, Outcome::Unclear(reason)) => Verdict::Inconclusive(reason),
    }
}

fn classify(exited_ok: Option<bool>, output: &str) -> Outcome {
    if output.contains(REFUSAL_TEXT) {
        return Outcome::Refused;
    }
    match exited_ok {
        Some(true) => Outcome::Served,
        Some(false) => Outcome::Unclear(
            output
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("opencode run failed with no output")
                .chars()
                .take(200)
                .collect(),
        ),
        None => Outcome::Unclear("opencode run timed out".to_string()),
    }
}

fn log(model: &str, egress_slot: Option<usize>, lane_tried: bool, verdict: &Verdict) {
    match verdict {
        Verdict::Upstream => tracing::warn!(
            event = "zen_gate_probe",
            verdict = "upstream",
            model = %model,
            egress_slot = ?egress_slot,
            lane_tried,
            "stock OpenCode is refused too: the free tier is down on OpenCode's side, not a proxy fault"
        ),
        Verdict::Proxy => tracing::error!(
            event = "zen_gate_probe",
            verdict = "proxy",
            model = %model,
            egress_slot = ?egress_slot,
            lane_tried,
            "stock OpenCode is served through the same lane while the proxy is refused: compare its \
             request with ours (tool names, stream flag, headers)"
        ),
        Verdict::Lane => tracing::error!(
            event = "zen_gate_probe",
            verdict = "lane",
            model = %model,
            egress_slot = ?egress_slot,
            lane_tried,
            "stock OpenCode is refused through this lane but served from this machine: Zen blocks \
             the lane's exit address; rotate it"
        ),
        Verdict::ProxyOrLane => tracing::error!(
            event = "zen_gate_probe",
            verdict = "proxy_or_lane",
            model = %model,
            egress_slot = ?egress_slot,
            lane_tried,
            "stock OpenCode is served from this machine, but the lane could not be tried: either our \
             request shape drifted or the lane's exit address is blocked"
        ),
        Verdict::Inconclusive(reason) => tracing::warn!(
            event = "zen_gate_probe",
            verdict = "inconclusive",
            model = %model,
            egress_slot = ?egress_slot,
            lane_tried,
            reason = %reason,
            "could not tell whether Zen refuses stock OpenCode too"
        ),
    }
}

fn claim() -> bool {
    let state = PROBE_STATE.get_or_init(|| Mutex::new((false, None)));
    let Ok(mut guard) = state.lock() else {
        return false;
    };
    if guard.0 || guard.1.is_some_and(|at| at.elapsed() < PROBE_MIN_INTERVAL) {
        return false;
    }
    *guard = (true, Some(Instant::now()));
    true
}

fn release() {
    if let Some(state) = PROBE_STATE.get()
        && let Ok(mut guard) = state.lock()
    {
        guard.0 = false;
    }
}

/// The probe's own HOME, kept between runs so only the first one pays for
/// the catalog download.
fn probe_home() -> PathBuf {
    let state = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| s.starts_with('/'))
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            format!("{home}/.local/state")
        });
    PathBuf::from(state).join("headroom").join("zen-probe-home")
}

/// One run, retried once when unclear: the CLI's first run in a fresh HOME
/// fails with `UnknownError` and the next one works (measured 2026-10-15,
/// opencode 1.18.34).
fn run_settled(bin: &Path, model: &str, proxy: Option<&str>) -> Outcome {
    match run(bin, model, proxy) {
        Outcome::Unclear(_) => run(bin, model, proxy),
        outcome => outcome,
    }
}

fn run(bin: &Path, model: &str, proxy: Option<&str>) -> Outcome {
    let home = probe_home();
    if let Err(e) = std::fs::create_dir_all(&home) {
        return Outcome::Unclear(format!("cannot create {}: {e}", home.display()));
    }
    let out_path = home.join("last-run.log");
    let Ok(out) = std::fs::File::create(&out_path) else {
        return Outcome::Unclear(format!("cannot write {}", out_path.display()));
    };
    let Ok(err) = out.try_clone() else {
        return Outcome::Unclear("cannot share the output file".to_string());
    };
    // A clean environment: no provider keys, no base-URL overrides, nothing
    // pointing the CLI back at this proxy.
    let mut command = std::process::Command::new(bin);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .current_dir(&home)
        .args([
            "run",
            "--pure",
            "-m",
            &format!("opencode/{model}"),
            "Reply with the single word: ok",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    if let Some(proxy) = proxy {
        command.env("HTTPS_PROXY", proxy).env("HTTP_PROXY", proxy);
    }
    let Ok(mut child) = command.spawn() else {
        return Outcome::Unclear(format!("cannot start {}", bin.display()));
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let exited_ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status.success()),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(500));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = std::fs::read_to_string(&out_path).unwrap_or_default();
    classify(exited_ok, &output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_needs_both_status_and_error_type() {
        let body = r#"{"type":"error","error":{"type":"FreeTierError","message":"..."}}"#;
        assert!(is_free_tier_refusal(http::StatusCode::FORBIDDEN, body));
        assert!(!is_free_tier_refusal(
            http::StatusCode::TOO_MANY_REQUESTS,
            body
        ));
        assert!(!is_free_tier_refusal(
            http::StatusCode::FORBIDDEN,
            r#"{"error":{"type":"RegionError"}}"#
        ));
    }

    /// The CLI prints the refusal and exits non-zero (OpenCode issue #52907
    /// shows the text); a served run exits zero.
    #[test]
    fn classify_reads_the_cli_outcome() {
        let refused = "Error: Error from provider (Console): OpenCode's free tier can only be used from within OpenCode";
        assert_eq!(classify(Some(false), refused), Outcome::Refused);
        assert_eq!(classify(Some(true), "> build · m\nok\n"), Outcome::Served);
        assert_eq!(
            classify(Some(false), "Error: rate limited\n\n"),
            Outcome::Unclear("Error: rate limited".to_string())
        );
        assert_eq!(
            classify(None, ""),
            Outcome::Unclear("opencode run timed out".to_string())
        );
    }

    #[test]
    fn decide_separates_outage_lane_and_proxy() {
        let unclear = || Outcome::Unclear("x".to_string());
        let no_direct_run = || -> Outcome { panic!("served through the lane needs no direct run") };
        assert_eq!(decide(Some(Outcome::Served), no_direct_run), Verdict::Proxy);
        assert_eq!(
            decide(Some(Outcome::Refused), || Outcome::Refused),
            Verdict::Upstream
        );
        assert_eq!(
            decide(Some(Outcome::Refused), || Outcome::Served),
            Verdict::Lane
        );
        assert_eq!(decide(None, || Outcome::Served), Verdict::ProxyOrLane);
        assert_eq!(
            decide(Some(unclear()), || Outcome::Served),
            Verdict::ProxyOrLane
        );
        assert_eq!(decide(None, || Outcome::Refused), Verdict::Upstream);
        assert_eq!(
            decide(Some(Outcome::Refused), unclear),
            Verdict::Inconclusive("x".to_string())
        );
    }

    /// Only the relay's own loopback lanes speak HTTP `CONNECT`; a remote or
    /// credentialed SOCKS proxy would not, and its credentials must not reach
    /// the CLI's environment.
    #[test]
    fn only_relay_lanes_become_http_proxies() {
        assert_eq!(
            http_lane("socks5h://127.0.0.1:18601").as_deref(),
            Some("http://127.0.0.1:18601")
        );
        assert_eq!(http_lane("socks5h://user:pass@127.0.0.1:18601"), None);
        assert_eq!(http_lane("socks5h://nl1.example.com:1080"), None);
        assert_eq!(http_lane("http://127.0.0.1:8080"), None);
    }

    #[test]
    fn probes_are_spaced_and_never_overlap() {
        assert!(claim());
        assert!(!claim(), "a probe is in flight");
        release();
        assert!(!claim(), "the last probe is too recent");
    }
}
