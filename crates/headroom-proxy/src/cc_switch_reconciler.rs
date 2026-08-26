//! cc-switch reconciler: keep Headroom in the request path without fighting cc-switch.
//!
//! cc-switch (https://github.com/farion1231/cc-switch) overwrites
//! `~/.claude/settings.json` on every provider switch, blowing away
//! `ANTHROPIC_BASE_URL`. This watcher detects the overwrite, captures
//! the real provider endpoint, and rewrites the URL back to Headroom.
//!
//! Gated behind `HEADROOM_CC_SWITCH_RECONCILE=1` — off by default.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::RwLock;
use url::Url;

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(300);

/// Check if the cc-switch reconciler is enabled via env var.
pub fn reconciler_enabled() -> bool {
    crate::ssl_context::cc_switch_reconciler_enabled()
}

/// Check if routing Claude Official through Headroom is opted in.
pub fn route_official() -> bool {
    crate::ssl_context::cc_switch_route_official()
}

/// Resolve the settings.json path, mirroring Python's `_settings_path()`.
fn settings_path() -> PathBuf {
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| dirs().join(".claude").to_string_lossy().into_owned());
    Path::new(&base).join("settings.json")
}

#[cfg(unix)]
fn dirs() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

#[cfg(not(unix))]
fn dirs() -> PathBuf {
    PathBuf::from(
        std::env::var("USERPROFILE")
            .or_else(|_| {
                std::env::var("HOMEDRIVE")
                    .map(|d| format!("{d}\\{}", std::env::var("HOMEPATH").unwrap_or_default()))
            })
            .unwrap_or_else(|_| "C:\\Users\\Default".into()),
    )
}

/// The captured upstream URL, shared between the reconciler and request path.
pub type DynamicUpstream = Arc<RwLock<Option<Url>>>;

/// Create a new empty dynamic upstream slot.
pub fn new_dynamic_upstream() -> DynamicUpstream {
    Arc::new(RwLock::new(None))
}

/// Poll-based watcher that keeps Headroom in the request path.
pub struct CCSwitchReconciler {
    proxy_url: String,
    default_upstream: String,
    dynamic_upstream: DynamicUpstream,
    route_official: bool,
    path: PathBuf,
    /// For skipping a reconcile when the file has not changed. The loop
    /// re-reads unconditionally today.
    #[allow(dead_code)]
    last_mtime_ns: Option<i64>,
    running: Arc<AtomicBool>,
}

impl CCSwitchReconciler {
    pub fn new(
        proxy_url: String,
        default_upstream: String,
        dynamic_upstream: DynamicUpstream,
        route_official: bool,
        path: Option<PathBuf>,
    ) -> Self {
        Self {
            proxy_url: proxy_url.trim_end_matches('/').to_string(),
            default_upstream,
            dynamic_upstream,
            route_official,
            path: path.unwrap_or_else(settings_path),
            last_mtime_ns: None,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start the background polling loop.
    pub fn start(&self) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let proxy_url = self.proxy_url.clone();
        let default_upstream = self.default_upstream.clone();
        let dynamic_upstream = Arc::clone(&self.dynamic_upstream);
        let route_official = self.route_official;
        let path = self.path.clone();
        let running = Arc::clone(&self.running);

        tracing::info!(
            path = %path.display(),
            proxy_url = %proxy_url,
            route_official,
            "cc-switch reconciler: watching settings.json"
        );

        let last_mtime: Arc<std::sync::Mutex<Option<i64>>> = Arc::new(std::sync::Mutex::new(None));

        // `tick()` uses `RwLock::blocking_write()`, which panics on a tokio
        // runtime worker thread — run the poll loop on a blocking thread.
        tokio::task::spawn_blocking(move || {
            let mut reconciler = Inner {
                proxy_url,
                default_upstream,
                dynamic_upstream,
                route_official,
                path,
                last_mtime_ns: last_mtime,
            };
            while running.load(Ordering::SeqCst) {
                // Python wraps tick() in try/except with the comment
                // "watcher must never die." Match that resilience here.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    reconciler.tick();
                }));
                std::thread::sleep(POLL_INTERVAL);
            }
        });
    }

    /// Stop the background polling loop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

struct Inner {
    proxy_url: String,
    default_upstream: String,
    dynamic_upstream: DynamicUpstream,
    route_official: bool,
    path: PathBuf,
    last_mtime_ns: Arc<std::sync::Mutex<Option<i64>>>,
}

impl Inner {
    /// One reconcile pass. Returns true if it rewrote settings.json.
    fn tick(&mut self) -> bool {
        let mtime_ns = match std::fs::metadata(&self.path) {
            Ok(m) => m_modified_ns(&m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) => return false,
        };

        {
            let last = self.last_mtime_ns.lock().unwrap();
            if *last == Some(mtime_ns) {
                return false;
            }
        }

        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(_) => return false, // transient read failure; retry next tick
        };

        let data: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return false, // transient parse failure; retry next tick
        };

        // Read succeeded: mark this mtime processed.
        {
            let mut last = self.last_mtime_ns.lock().unwrap();
            *last = Some(mtime_ns);
        }

        let obj = match data.as_object() {
            Some(o) => o,
            None => return false,
        };

        let env = obj
            .get("env")
            .and_then(|e| e.as_object())
            .cloned()
            .unwrap_or_default();

        let url = env
            .get("ANTHROPIC_BASE_URL")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Empty / official: cc-switch wrote {"env": {}} (Claude Official, OAuth).
        if url.is_empty() {
            if self.route_official {
                if let Ok(upstream) = Url::parse(&self.default_upstream) {
                    *self.dynamic_upstream.blocking_write() = Some(upstream);
                }
                let mut new_env = env.clone();
                new_env.insert(
                    "ANTHROPIC_BASE_URL".into(),
                    Value::String(self.proxy_url.clone()),
                );
                let mut new_data = obj.clone();
                new_data.insert("env".into(), Value::Object(new_env));
                self.atomic_write(&Value::Object(new_data));
                tracing::info!("cc-switch reconciler: official -> route via Headroom");
                return true;
            }
            return false;
        }

        // Already pointing at us: nothing to do (loop guard).
        if url.trim_end_matches('/') == self.proxy_url {
            return false;
        }

        // Third-party / custom endpoint: capture it as upstream, point Claude at us.
        let upstream = match Url::parse(&url) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(
                    captured_upstream = %url,
                    error = %e,
                    "cc-switch reconciler: captured URL is not valid; skipping rewrite to avoid broken upstream"
                );
                return false;
            }
        };
        *self.dynamic_upstream.blocking_write() = Some(upstream);
        let mut new_env = env.clone();
        new_env.insert(
            "ANTHROPIC_BASE_URL".into(),
            Value::String(self.proxy_url.clone()),
        );
        let mut new_data = obj.clone();
        new_data.insert("env".into(), Value::Object(new_env));
        self.atomic_write(&Value::Object(new_data));
        tracing::info!(
            captured_upstream = %url,
            proxy_url = %self.proxy_url,
            "cc-switch reconciler: captured upstream, base_url -> proxy"
        );
        true
    }

    /// Atomic write: temp file + os.replace, matching Python's `_atomic_write`.
    fn atomic_write(&self, data: &Value) {
        let pid = std::process::id();
        let tmp = self
            .path
            .with_file_name(format!("settings.json.{pid}.hrtmp"));
        let json = serde_json::to_string_pretty(data).unwrap_or_default();
        if let Err(e) = std::fs::write(&tmp, format!("{json}\n")) {
            tracing::debug!(error = %e, "cc-switch reconciler: temp write failed");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            tracing::debug!(error = %e, "cc-switch reconciler: atomic rename failed");
            return;
        }
        // Skip the mtime bump caused by our own write so we don't re-process it.
        if let Ok(m) = std::fs::metadata(&self.path) {
            let mut last = self.last_mtime_ns.lock().unwrap();
            *last = Some(m_modified_ns(&m));
        }
    }
}

/// Get mtime as nanoseconds since epoch, matching Python's `st_mtime_ns`.
fn m_modified_ns(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_inner(tmp: &Path, route_official: bool) -> (Inner, DynamicUpstream) {
        let du = new_dynamic_upstream();
        let inner = Inner {
            proxy_url: "http://127.0.0.1:8787".into(),
            default_upstream: "https://api.anthropic.com".into(),
            dynamic_upstream: Arc::clone(&du),
            route_official,
            path: tmp.join("settings.json"),
            last_mtime_ns: Arc::new(std::sync::Mutex::new(None)),
        };
        (inner, du)
    }

    use std::sync::atomic::AtomicI64;

    static COUNTER: AtomicI64 = AtomicI64::new(0);

    fn write_settings(tmp: &Path, val: Value) {
        let path = tmp.join("settings.json");
        std::fs::write(&path, serde_json::to_string(&val).unwrap()).unwrap();
        // Guarantee mtime advances: use a monotonic counter to produce a
        // unique mtime on every call, regardless of filesystem resolution.
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let ts = filetime::FileTime::from_unix_time(1_700_000_000 + seq, 0);
        filetime::set_file_mtime(&path, ts).unwrap();
    }

    #[test]
    fn test_third_party_captured_and_base_url_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut r, du) = make_inner(tmp.path(), false);
        write_settings(
            tmp.path(),
            json!({"env": {"ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic", "ANTHROPIC_AUTH_TOKEN": "sk-x", "ANTHROPIC_MODEL": "deepseek"}}),
        );
        assert!(r.tick());
        let stored = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
        let env = serde_json::from_str::<Value>(&stored).unwrap()["env"].clone();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "sk-x");
        assert_eq!(env["ANTHROPIC_MODEL"], "deepseek");
        let captured = du.blocking_read();
        assert_eq!(
            captured.as_ref().unwrap().as_str(),
            "https://api.deepseek.com/anthropic"
        );
    }

    #[test]
    fn test_no_rewrite_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut r, _du) = make_inner(tmp.path(), false);
        write_settings(
            tmp.path(),
            json!({"env": {"ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic"}}),
        );
        assert!(r.tick());
        // Atomic write rewrites base_url to proxy; second tick must be a no-op.
        assert!(!r.tick());
    }

    #[test]
    fn test_switching_provider_recaptures() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut r, du) = make_inner(tmp.path(), false);
        write_settings(
            tmp.path(),
            json!({"env": {"ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic", "ANTHROPIC_AUTH_TOKEN": "sk-d"}}),
        );
        assert!(r.tick());
        write_settings(
            tmp.path(),
            json!({"env": {"ANTHROPIC_BASE_URL": "https://api.kimi.com/anthropic", "ANTHROPIC_AUTH_TOKEN": "sk-k"}}),
        );
        assert!(r.tick());
        let captured = du.blocking_read();
        assert_eq!(
            captured.as_ref().unwrap().as_str(),
            "https://api.kimi.com/anthropic"
        );
        let stored = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stored).unwrap()["env"]["ANTHROPIC_AUTH_TOKEN"],
            "sk-k"
        );
    }

    #[test]
    fn test_official_left_direct_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut r, _du) = make_inner(tmp.path(), false);
        write_settings(tmp.path(), json!({"env": {}}));
        assert!(!r.tick());
        let stored = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stored).unwrap()["env"],
            json!({})
        );
    }

    #[test]
    fn test_official_routed_when_opted_in() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut r, du) = make_inner(tmp.path(), true);
        write_settings(tmp.path(), json!({"env": {}}));
        assert!(r.tick());
        let stored = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stored).unwrap()["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:8787"
        );
        let captured = du.blocking_read();
        assert_eq!(
            captured.as_ref().unwrap().as_str(),
            "https://api.anthropic.com/"
        );
    }

    #[test]
    fn test_missing_file_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut r, _du) = make_inner(tmp.path(), false);
        assert!(!r.tick());
    }

    #[test]
    fn test_non_string_base_url_does_not_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut r, _du) = make_inner(tmp.path(), false);
        write_settings(tmp.path(), json!({"env": {"ANTHROPIC_BASE_URL": 1234}}));
        assert!(!r.tick());
    }

    #[test]
    fn test_transient_invalid_json_retries_next_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut r, _du) = make_inner(tmp.path(), false);
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, "{not valid json").unwrap();
        assert!(!r.tick());
        assert!(r.last_mtime_ns.lock().unwrap().is_none());
        write_settings(
            tmp.path(),
            json!({"env": {"ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic"}}),
        );
        assert!(r.tick());
    }
}
