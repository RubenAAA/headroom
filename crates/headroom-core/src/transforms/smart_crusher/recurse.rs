//! Item-by-item descent into an array, shared by the below-threshold path
//! and the adaptive-limit path of [`SmartCrusher::process_value`].

use serde_json::Value;

use super::crusher::{ProseHook, SmartCrusher};

impl SmartCrusher {
    /// Process every item one level deeper, collecting non-empty info strings.
    ///
    /// Adaptive sizing can keep every row even after the analysis threshold is
    /// crossed; that path calls this too, so nested values still get their own
    /// safe transforms (upstream `7790bdee`).
    pub(super) fn recurse_items(
        &self,
        arr: &[Value],
        depth: usize,
        query_context: &str,
        bias: f64,
        prose_hook: Option<&ProseHook<'_>>,
        info_parts: &mut Vec<String>,
    ) -> Value {
        let mut processed: Vec<Value> = Vec::with_capacity(arr.len());
        for item in arr {
            let (p_item, p_info) =
                self.process_value_with_hook(item, depth + 1, query_context, bias, prose_hook);
            processed.push(p_item);
            if !p_info.is_empty() {
                info_parts.push(p_info);
            }
        }
        Value::Array(processed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::SmartCrusherConfig;
    use super::*;
    use serde_json::json;

    fn crusher() -> SmartCrusher {
        SmartCrusher::new(SmartCrusherConfig::default())
    }

    #[test]
    fn crush_recurses_into_object_arrays_at_adaptive_limit() {
        let c = crusher();
        let description =
            "Curated collection of business tables covering customer, product, ".repeat(12);

        for n in 5..=8 {
            let rows: Vec<Value> = (0..n)
                .map(|i| {
                    json!({
                        "name": format!("domain_{i}"),
                        "description": description,
                        "tables": 12 + i,
                        "owner": "data-platform"
                    })
                })
                .collect();
            let input = json!({"domains": rows}).to_string();
            let result = c.crush(&input, "list the available domains", 1.0);

            assert!(
                result.strategy.contains("string_ccr:"),
                "n={n} should recurse into rows at the adaptive limit: strategy={} output={}",
                result.strategy,
                result.compressed
            );
            assert!(
                result.compressed.contains("<<ccr:"),
                "n={n} should offload long row strings: {}",
                result.compressed
            );
        }
    }
}
