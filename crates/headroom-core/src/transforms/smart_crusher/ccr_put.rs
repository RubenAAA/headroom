//! CCR writes for SmartCrusher's row and item offloads. Lives outside
//! `crusher.rs` because that file is pinned at its line count
//! (`scripts/file-size-baseline.txt`).

use serde_json::Value;

use super::crusher::{CrushArrayResult, SmartCrusher};

impl SmartCrusher {
    /// Stores `payload` under `hash` when a store is configured. `false`
    /// means the store refused it: a marker for that hash would point the
    /// model at content it cannot fetch, so the caller keeps the original.
    pub(super) fn ccr_put(&self, hash: &str, payload: &str) -> bool {
        let Some(store) = &self.ccr_store else {
            return true;
        };
        if store.put(hash, payload) {
            return true;
        }
        tracing::warn!(
            event = "ccr_put_failed",
            target = "ccr.crusher",
            hash = %hash,
            "ccr_put_failed; keeping the original items"
        );
        false
    }
}

impl CrushArrayResult {
    /// Every row kept, no marker: the result when the CCR store refused
    /// the original.
    pub(super) fn kept_whole(items: &[Value]) -> Self {
        Self {
            items: items.to_vec(),
            strategy_info: "skip:ccr_put_failed".to_string(),
            ccr_hash: None,
            dropped_summary: String::new(),
            compacted: None,
            compaction_kind: None,
            constant_fields_sentinel: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use crate::ccr::RefusingCcrStore;
    use crate::transforms::smart_crusher::{SmartCrusher, SmartCrusherBuilder, SmartCrusherConfig};

    fn refusing(config: SmartCrusherConfig) -> SmartCrusher {
        SmartCrusherBuilder::new(config)
            .with_ccr_store(Arc::new(RefusingCcrStore))
            .build()
    }

    #[test]
    fn refused_row_offload_keeps_every_row() {
        // A marker whose original never reached the store points the
        // model at a hash it cannot fetch, and the dropped rows are lost.
        let c = refusing(SmartCrusherConfig {
            lossless_min_savings_ratio: 0.99, // force lossy path
            ..SmartCrusherConfig::default()
        });
        let items: Vec<Value> = (0..50).map(|_| json!({"status": "ok"})).collect();
        let result = c.crush_array(&items, "", 1.0);
        assert_eq!(result.items, items);
        assert!(result.ccr_hash.is_none());
        assert!(result.dropped_summary.is_empty());
    }

    #[test]
    fn refused_scalar_offload_keeps_every_item() {
        let c = refusing(SmartCrusherConfig::default());
        let items: Vec<Value> = (0..120).map(|i| json!(format!("r{i}"))).collect();
        let out = c.crush(&serde_json::to_string(&items).unwrap(), "", 1.0);
        let kept: Value = serde_json::from_str(&out.compressed).expect("output is JSON");
        assert_eq!(kept, Value::Array(items));
    }

    #[test]
    fn refused_opaque_string_stays_as_it_came() {
        let c = refusing(SmartCrusherConfig::default());
        let big_b64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=".repeat(8);
        let doc = json!({"id": 1, "blob": big_b64});
        let (out, _info) = c.process_value(&doc, 0, "", 1.0);
        assert_eq!(out, doc);
    }
}
