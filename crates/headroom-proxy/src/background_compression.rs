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

        let pending_clone = pending.clone();
        let stats_clone = stats.clone();

        tokio::spawn(async move {
            Self::drain_loop(rx, pending_clone, stats_clone).await;
        });

        Self {
            tx,
            pending,
            stats,
            max_queue,
        }
    }

    /// Queue a compression job. Returns false (and drops) if the key is
    /// already in flight or the queue is full.
    pub async fn enqueue(&self, job: CompressionJob) -> bool {
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
