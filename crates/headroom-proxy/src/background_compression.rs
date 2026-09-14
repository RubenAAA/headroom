//! Off-path background compression.
//!
//! When a cold-start-large request would run kompress synchronously, it
//! instead forwards immediately and enqueues compression here. A single
//! per-process drain runs it with no request-coupled deadline and stores
//! the result in the session compression cache.
//!
//! This module provides the queue and drain logic. The actual compressor
//! and cache store are injected via traits.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};

/// A compression job to be executed off the request path.
pub struct CompressionJob {
    pub key: String,
    pub compress: Box<dyn Fn() -> Vec<u8> + Send + Sync>,
    pub store: Box<dyn Fn(Vec<u8>) + Send + Sync>,
}

/// Stats for the background compressor.
#[derive(Debug, Default, Clone)]
pub struct BackgroundStats {
    pub queued: usize,
    pub pending: usize,
    pub processed: u64,
    pub dropped: u64,
    pub errors: u64,
}

/// Single per-process async drain that compresses enqueued work off the
/// request path, with no request-coupled deadline.
pub struct BackgroundCompressor {
    tx: mpsc::Sender<CompressionJob>,
    pending: Arc<Mutex<HashSet<String>>>,
    stats: Arc<Mutex<BackgroundStats>>,
    /// Receiver parked here until the first `enqueue` spawns the drain.
    /// `new()` must stay callable outside a Tokio runtime (MINOR-055:
    /// the old eager `tokio::spawn` panicked there); the drain starts
    /// lazily where a runtime is guaranteed.
    rx: std::sync::Mutex<Option<mpsc::Receiver<CompressionJob>>>,
    drain_started: AtomicBool,
    /// The bound the channel was built with, kept for a queue-depth report
    /// that does not exist yet.
    #[allow(dead_code)]
    max_queue: usize,
}

impl BackgroundCompressor {
    pub fn new(max_queue: usize) -> Self {
        let (tx, rx) = mpsc::channel(max_queue);
        let pending = Arc::new(Mutex::new(HashSet::new()));
        let stats = Arc::new(Mutex::new(BackgroundStats::default()));

        Self {
            tx,
            pending,
            stats,
            rx: std::sync::Mutex::new(Some(rx)),
            drain_started: AtomicBool::new(false),
            max_queue,
        }
    }

    /// Start the drain loop once, on first use. Returns false when there
    /// is no Tokio runtime to spawn onto — the caller (`enqueue`) then
    /// reports the job dropped, the same as a full queue, instead of
    /// panicking.
    fn ensure_drain(&self) -> bool {
        if self.drain_started.swap(true, Ordering::SeqCst) {
            return true;
        }
        let rx = self.rx.lock().ok().and_then(|mut g| g.take());
        let Some(rx) = rx else {
            return false;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // No runtime: park the receiver back and allow a later call
            // (possibly from a runtime) to retry the spawn.
            if let Ok(mut g) = self.rx.lock() {
                *g = Some(rx);
            }
            self.drain_started.store(false, Ordering::SeqCst);
            return false;
        };
        let pending_clone = self.pending.clone();
        let stats_clone = self.stats.clone();
        handle.spawn(async move {
            Self::drain_loop(rx, pending_clone, stats_clone).await;
        });
        true
    }

    /// Queue a compression job. Returns false (and drops) if the key is
    /// already in flight, the queue is full, or no async runtime exists
    /// to drain the queue (see `ensure_drain`).
    pub async fn enqueue(&self, job: CompressionJob) -> bool {
        if !self.ensure_drain() {
            return false;
        }
        let key = job.key.clone();

        // Claim the slot BEFORE the job is observable
        {
            let mut pending = self.pending.lock().await;
            if pending.contains(&key) {
                return false; // already queued / in flight
            }
            pending.insert(key.clone());
        }

        match self.tx.try_send(job) {
            Ok(()) => true,
            Err(_) => {
                let mut pending = self.pending.lock().await;
                pending.remove(&key);
                let mut stats = self.stats.lock().await;
                stats.dropped += 1;
                false
            }
        }
    }

    async fn drain_loop(
        mut rx: mpsc::Receiver<CompressionJob>,
        pending: Arc<Mutex<HashSet<String>>>,
        stats: Arc<Mutex<BackgroundStats>>,
    ) {
        while let Some(job) = rx.recv().await {
            let key = job.key.clone();
            match tokio::task::spawn_blocking(move || (job.compress)()).await {
                Ok(result) => {
                    (job.store)(result);
                    let mut s = stats.lock().await;
                    s.processed += 1;
                }
                Err(e) => {
                    let mut s = stats.lock().await;
                    s.errors += 1;
                    tracing::warn!("background compression failed for {key}: {e}");
                }
            }
            pending.lock().await.remove(&key);
        }
    }

    /// Get current stats.
    pub async fn stats(&self) -> BackgroundStats {
        let mut s = self.stats.lock().await.clone();
        s.queued = self.tx.max_capacity() - self.tx.capacity();
        s.pending = self.pending.lock().await.len();
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn enqueue_and_process() {
        let compressor = BackgroundCompressor::new(10);
        let result = Arc::new(AtomicUsize::new(0));
        let result_clone = result.clone();

        let job = CompressionJob {
            key: "test-key".to_string(),
            compress: Box::new(|| vec![1, 2, 3]),
            store: Box::new(move |_data| {
                result_clone.fetch_add(1, Ordering::SeqCst);
            }),
        };

        assert!(compressor.enqueue(job).await);
        // Give the drain loop time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(result.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dedup_rejects_duplicate_key() {
        let compressor = BackgroundCompressor::new(10);
        let _noop = Arc::new(AtomicUsize::new(0));

        let job1 = CompressionJob {
            key: "dup".to_string(),
            compress: Box::new(|| vec![]),
            store: Box::new(|_| {}),
        };
        let job2 = CompressionJob {
            key: "dup".to_string(),
            compress: Box::new(|| vec![]),
            store: Box::new(|_| {}),
        };

        assert!(compressor.enqueue(job1).await);
        assert!(!compressor.enqueue(job2).await);
    }

    /// MINOR-055: construction must not require a Tokio runtime (the old
    /// eager spawn panicked here). Plain #[test] has no runtime.
    #[test]
    fn new_outside_runtime_does_not_panic() {
        let compressor = BackgroundCompressor::new(10);
        assert!(!compressor.drain_started.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn stats_tracks_processed() {
        let compressor = BackgroundCompressor::new(10);
        let job = CompressionJob {
            key: "s1".to_string(),
            compress: Box::new(|| vec![]),
            store: Box::new(|_| {}),
        };
        compressor.enqueue(job).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let s = compressor.stats().await;
        assert_eq!(s.processed, 1);
    }
}
