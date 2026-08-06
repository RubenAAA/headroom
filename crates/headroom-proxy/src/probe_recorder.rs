//! Opt-in JSONL recorder for compression events (probe-based replay evals).
//!
//! Records (original, compressed) message pairs at `INPUT_COMPRESSED` so that
//! `headroom evals probes` can measure what compression removed from real
//! proxied sessions. Activated only when `HEADROOM_PROBE_RECORD_DIR` is set.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

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
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
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
            tracing::warn!("probe recorder disabled ({RECORD_DIR_ENV}): {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        std::env::remove_var(RECORD_DIR_ENV);
        assert!(probe_recorder_from_env().is_none());
    }
}
