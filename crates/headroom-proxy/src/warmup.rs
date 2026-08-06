//! Warmup registry for proxy cold-start state.
//!
//! Holds references to preloaded heavy assets (ML compressors, content detectors,
//! memory backends, embedders) that are eagerly initialized during startup.

use std::collections::HashMap;

/// Status of a warmup slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarmupStatus {
    Loaded,
    Loading,
    Null,
    Error,
}

impl std::fmt::Display for WarmupStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WarmupStatus::Loaded => write!(f, "loaded"),
            WarmupStatus::Loading => write!(f, "loading"),
            WarmupStatus::Null => write!(f, "null"),
            WarmupStatus::Error => write!(f, "error"),
        }
    }
}

/// Status record for one warmed-up component.
#[derive(Debug, Clone)]
pub struct WarmupSlot {
    pub status: WarmupStatus,
    pub error: Option<String>,
    pub info: HashMap<String, String>,
}

impl Default for WarmupSlot {
    fn default() -> Self {
        Self {
            status: WarmupStatus::Null,
            error: None,
            info: HashMap::new(),
        }
    }
}

impl WarmupSlot {
    pub fn mark_loaded(&mut self, info: Option<HashMap<String, String>>) {
        self.status = WarmupStatus::Loaded;
        self.error = None;
        if let Some(i) = info {
            self.info.extend(i);
        }
    }

    pub fn mark_loading(&mut self) {
        self.status = WarmupStatus::Loading;
        self.error = None;
    }

    pub fn mark_null(&mut self) {
        self.status = WarmupStatus::Null;
        self.error = None;
    }

    pub fn mark_error(&mut self, error: &str) {
        self.status = WarmupStatus::Error;
        self.error = Some(error.to_string());
    }

    /// Serialize for /debug/warmup. Never includes the raw handle.
    pub fn to_dict(&self) -> HashMap<String, serde_json::Value> {
        let mut payload = HashMap::new();
        payload.insert(
            "status".to_string(),
            serde_json::json!(self.status.to_string()),
        );
        if let Some(ref e) = self.error {
            payload.insert("error".to_string(), serde_json::json!(e));
        }
        if !self.info.is_empty() {
            let info_map: serde_json::Map<String, serde_json::Value> = self
                .info
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::json!(v)))
                .collect();
            payload.insert("info".to_string(), serde_json::Value::Object(info_map));
        }
        payload
    }
}

/// Shared preloaded asset registry populated by startup.
#[derive(Debug, Clone, Default)]
pub struct WarmupRegistry {
    pub kompress: WarmupSlot,
    pub magika: WarmupSlot,
    pub code_aware: WarmupSlot,
    pub tree_sitter: WarmupSlot,
    pub smart_crusher: WarmupSlot,
    pub memory_backend: WarmupSlot,
    pub memory_embedder: WarmupSlot,
}

impl WarmupRegistry {
    /// Merge a `eager_load_compressors` status dict into slots.
    pub fn merge_transform_status(&mut self, status: &HashMap<String, String>) {
        let apply = |slot: &mut WarmupSlot, value: Option<&String>| {
            if let Some(v) = value {
                let v_lower = v.trim().to_lowercase();
                if v_lower == "enabled" || v_lower == "ready" || v_lower.starts_with("loaded") {
                    if slot.status != WarmupStatus::Loaded {
                        slot.mark_loaded(None);
                        slot.info.insert("source_status".to_string(), v.clone());
                    } else {
                        slot.info
                            .entry("source_status".to_string())
                            .or_insert_with(|| v.clone());
                    }
                } else if slot.status != WarmupStatus::Loaded {
                    slot.info.insert("source_status".to_string(), v.clone());
                }
            }
        };

        apply(&mut self.kompress, status.get("kompress"));
        apply(&mut self.magika, status.get("magika"));
        apply(&mut self.code_aware, status.get("code_aware"));
        apply(&mut self.tree_sitter, status.get("tree_sitter"));
        apply(&mut self.smart_crusher, status.get("smart_crusher"));
    }

    /// Serialize the whole registry (for /debug/warmup).
    pub fn to_dict(&self) -> HashMap<String, serde_json::Value> {
        let mut map = HashMap::new();
        map.insert(
            "kompress".to_string(),
            serde_json::json!(self.kompress.to_dict()),
        );
        map.insert(
            "magika".to_string(),
            serde_json::json!(self.magika.to_dict()),
        );
        map.insert(
            "code_aware".to_string(),
            serde_json::json!(self.code_aware.to_dict()),
        );
        map.insert(
            "tree_sitter".to_string(),
            serde_json::json!(self.tree_sitter.to_dict()),
        );
        map.insert(
            "smart_crusher".to_string(),
            serde_json::json!(self.smart_crusher.to_dict()),
        );
        map.insert(
            "memory_backend".to_string(),
            serde_json::json!(self.memory_backend.to_dict()),
        );
        map.insert(
            "memory_embedder".to_string(),
            serde_json::json!(self.memory_embedder.to_dict()),
        );
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_slot_is_null() {
        let slot = WarmupSlot::default();
        assert_eq!(slot.status, WarmupStatus::Null);
        assert!(slot.error.is_none());
    }

    #[test]
    fn mark_loaded_sets_status() {
        let mut slot = WarmupSlot::default();
        let mut info = HashMap::new();
        info.insert("source_status".to_string(), "enabled".to_string());
        slot.mark_loaded(Some(info));
        assert_eq!(slot.status, WarmupStatus::Loaded);
        assert_eq!(slot.info.get("source_status").unwrap(), "enabled");
    }

    #[test]
    fn mark_error_sets_status() {
        let mut slot = WarmupSlot::default();
        slot.mark_error("something broke");
        assert_eq!(slot.status, WarmupStatus::Error);
        assert_eq!(slot.error.as_deref(), Some("something broke"));
    }

    #[test]
    fn merge_transform_status_promotes_to_loaded() {
        let mut reg = WarmupRegistry::default();
        let mut status = HashMap::new();
        status.insert("kompress".to_string(), "enabled".to_string());
        status.insert("magika".to_string(), "ready".to_string());
        reg.merge_transform_status(&status);
        assert_eq!(reg.kompress.status, WarmupStatus::Loaded);
        assert_eq!(reg.magika.status, WarmupStatus::Loaded);
        assert_eq!(reg.code_aware.status, WarmupStatus::Null);
    }

    #[test]
    fn to_dict_includes_all_slots() {
        let reg = WarmupRegistry::default();
        let dict = reg.to_dict();
        assert!(dict.contains_key("kompress"));
        assert!(dict.contains_key("memory_embedder"));
    }
}
