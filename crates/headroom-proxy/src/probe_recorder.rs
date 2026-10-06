//! Opt-in JSONL recorder for compression events (probe-based replay evals).
//!
//! Records (original, compressed) message pairs at `INPUT_COMPRESSED` so that
//! `headroom evals probes` can measure what compression removed from real
//! proxied sessions. Activated only when `HEADROOM_PROBE_RECORD_DIR` is set.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const RECORD_DIR_ENV: &str = "HEADROOM_PROBE_RECORD_DIR";

/// A compression event to record.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CompressionEvent {
    pub ts: f64,
    pub request_id: String,
    pub provider: String,
    pub model: String,
    pub tokens_before: Option<u64>,
    pub tokens_after: Option<u64>,
    pub transforms_applied: Vec<String>,
}

/// Pipeline extension appending one JSONL line per compression event.
pub struct CompressionEventRecorder {
    path: PathBuf,
    lock: Mutex<()>,
}

impl CompressionEventRecorder {
    pub fn new(record_dir: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(record_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(record_dir, fs::Permissions::from_mode(0o700))?;
        }
        let pid = std::process::id();
        let path = record_dir.join(format!("compression-events-{pid}.jsonl"));
        // Narrow a file left by an older build: the pairs recorded here are
        // verbatim conversation content, so the file is owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        Ok(Self {
            path,
            lock: Mutex::new(()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record a compression event. Never raises.
    pub fn record(&self, event: &CompressionEvent) {
        let line = match serde_json::to_string(event) {
            Ok(l) => l,
            Err(_) => return,
        };
        let _guard = self.lock.lock();
        // Born 0600: the umask can only clear bits, never add them, so an
        // explicit mode is what makes this file owner-only on a stock host.
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(mut file) = options.open(&self.path) {
            let _ = writeln!(file, "{line}");
        }
    }
}

/// Build a recorder when `HEADROOM_PROBE_RECORD_DIR` is set, else None.
pub fn probe_recorder_from_env() -> Option<CompressionEventRecorder> {
    let dir = std::env::var(RECORD_DIR_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;

    let path = PathBuf::from(dir);
    match CompressionEventRecorder::new(&path) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(
                event = "probe_recorder_disabled",
                "probe recorder disabled ({RECORD_DIR_ENV}): {e}"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    #[test]
    fn is_auditable_path_logic() {
        // This is just a sanity check that the recorder compiles
        let dir = std::env::temp_dir().join("headroom_test_probe");
        let recorder = CompressionEventRecorder::new(&dir).unwrap();
        let event = CompressionEvent {
            ts: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs_f64(),
            request_id: "req-1".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-3".to_string(),
            tokens_before: Some(1000),
            tokens_after: Some(500),
            transforms_applied: vec!["kompress".to_string()],
        };
        recorder.record(&event);
        assert!(recorder.path().exists());
        let content = fs::read_to_string(recorder.path()).unwrap();
        assert!(content.contains("req-1"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_recorder_from_env_returns_none_when_unset() {
        unsafe { std::env::remove_var(RECORD_DIR_ENV) };
        assert!(probe_recorder_from_env().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn record_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        fn mode(p: &std::path::Path) -> u32 {
            std::fs::metadata(p).unwrap().permissions().mode() & 0o777
        }

        fn event() -> CompressionEvent {
            CompressionEvent {
                ts: 0.0,
                request_id: "req-1".to_string(),
                provider: "anthropic".to_string(),
                model: "claude-3".to_string(),
                tokens_before: Some(1000),
                tokens_after: Some(500),
                transforms_applied: vec!["kompress".to_string()],
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "headroom-probe-perms-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let recorder = CompressionEventRecorder::new(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);

        // A wide file left by an older build is narrowed on open.
        std::fs::write(recorder.path(), "").unwrap();
        std::fs::set_permissions(recorder.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let recorder = CompressionEventRecorder::new(&dir).unwrap();
        assert_eq!(mode(recorder.path()), 0o600);

        // And a fresh file is born owner-only.
        std::fs::remove_file(recorder.path()).unwrap();
        recorder.record(&event());
        assert_eq!(mode(recorder.path()), 0o600);

        let _ = fs::remove_dir_all(&dir);
    }
}
