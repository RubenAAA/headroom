//! Compression summary generator — describes what was dropped.
//!
//! When content is compressed, the LLM needs to know what it's missing.
//! Instead of just "[480 items omitted]", we generate a categorical summary:
//! "[480 items omitted: 150 log entries (3 with errors), 200 test results (12 failures)]"
//!
//! This helps the LLM decide whether to call headroom_retrieve and what to search for.

use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

// Fields that commonly indicate item category/type
const CATEGORY_FIELDS: &[&str] = &[
    "type",
    "status",
    "kind",
    "category",
    "level",
    "severity",
    "state",
    "phase",
    "action",
    "event_type",
    "log_level",
    "result",
    "outcome",
];

fn notable_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)error|fail|critical|warning|exception|crash|timeout|denied|rejected|invalid",
        )
        .unwrap()
    })
}

fn url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^https?://|^/[a-z]").unwrap())
}

fn func_def_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:def|func|fn|function)\s+(?:\([^)]*\)\s*)?(\w+)").unwrap())
}

fn method_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:public|private|protected|static|async|export)\s+.*?(\w+)\s*\(").unwrap()
    })
}

fn class_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"class\s+(\w+)").unwrap())
}

fn fallback_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\w+)\s*\(").unwrap())
}

/// Generate a categorical summary of items that were dropped.
pub fn summarize_dropped_items(
    all_items: &[Value],
    kept_items: &[Value],
    kept_indices: Option<&std::collections::HashSet<usize>>,
    max_categories: usize,
    max_notable: usize,
) -> String {
    if all_items.is_empty() || kept_items.len() >= all_items.len() {
        return String::new();
    }

    // Determine which items were dropped
    let dropped: Vec<&Value> = if let Some(indices) = kept_indices {
        all_items
            .iter()
            .enumerate()
            .filter(|(i, _)| !indices.contains(i))
            .map(|(_, item)| item)
            .collect()
    } else {
        let kept_keys: std::collections::HashSet<String> =
            kept_items.iter().map(item_key).collect();
        all_items
            .iter()
            .filter(|item| !kept_keys.contains(&item_key(item)))
            .collect()
    };

    if dropped.is_empty() {
        return String::new();
    }

    // Strategy 1: Categorize by type/status/kind fields
    let categories = categorize_by_fields(&dropped);

    // Strategy 2: Find notable items (errors, failures, warnings)
    let notable = find_notable_items(&dropped, max_notable);

    let mut parts = Vec::new();

    if !categories.is_empty() {
        let mut cat_vec: Vec<(&String, &usize)> = categories.iter().collect();
        cat_vec.sort_by(|a, b| b.1.cmp(a.1));
        let cat_strs: Vec<String> = cat_vec
            .iter()
            .take(max_categories)
            .map(|(val, count)| format!("{} {}", count, val))
            .collect();
        parts.push(cat_strs.join(", "));
    }

    if !notable.is_empty() {
        parts.push(format!("notable: {}", notable.join("; ")));
    }

    if parts.is_empty() {
        // Fallback: just describe the data shape
        let keys = common_keys(&dropped);
        if !keys.is_empty() {
            let shown: Vec<&str> = keys.iter().take(5).map(|s| s.as_str()).collect();
            parts.push(format!("fields: {}", shown.join(", ")));
        }
    }

    parts.join("; ")
}

/// Generate a summary of compressed code sections from AST data.
pub fn summarize_compressed_code(
    function_bodies: &[(String, String, usize)],
    compressed_bodies_count: usize,
) -> String {
    if function_bodies.is_empty() || compressed_bodies_count == 0 {
        return String::new();
    }

    // Extract short names from signatures
    let names: Vec<String> = function_bodies
        .iter()
        .filter_map(|(sig, _, _)| extract_name_from_signature(sig))
        .collect();

    if names.is_empty() {
        return format!("{} function bodies compressed", compressed_bodies_count);
    }

    // Show up to 6 names
    let shown: Vec<&str> = names.iter().take(6).map(|s| s.as_str()).collect();
    let mut result = format!(
        "{} bodies compressed: {}",
        compressed_bodies_count,
        shown.join(", ")
    );
    if names.len() > 6 {
        result.push_str(&format!(" (+{} more)", names.len() - 6));
    }
    result
}

/// Extract the function/method name from a signature string.
pub fn extract_name_from_signature(sig: &str) -> Option<String> {
    if let Some(caps) = func_def_re().captures(sig) {
        return Some(format!("{}()", &caps[1]));
    }
    if let Some(caps) = method_re().captures(sig) {
        return Some(format!("{}()", &caps[1]));
    }
    if let Some(caps) = class_re().captures(sig) {
        return Some(caps[1].to_string());
    }
    if let Some(caps) = fallback_re().captures(sig) {
        return Some(format!("{}()", &caps[1]));
    }
    None
}

// ---- Internal helpers ----

fn item_key(item: &Value) -> String {
    let obj = match item.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let parts: Vec<String> = obj
        .iter()
        .take(4)
        .map(|(k, v)| {
            let val_str = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => "null".to_string(),
                _ => v.to_string(),
            };
            format!("{}={}", k, val_str)
        })
        .collect();
    parts.join("|")
}

fn categorize_by_fields(items: &[&Value]) -> HashMap<String, usize> {
    let mut categories: HashMap<String, usize> = HashMap::new();

    for item in items {
        let obj = match item.as_object() {
            Some(o) => o,
            None => continue,
        };

        let mut categorized = false;
        for field in CATEGORY_FIELDS {
            if let Some(val) = obj.get(*field) {
                if let Some(s) = val.as_str() {
                    if s.len() < 50 {
                        let clean_val = s.replace('\n', " ").replace('\r', "").trim().to_string();
                        if !clean_val.is_empty() {
                            *categories.entry(clean_val).or_insert(0) += 1;
                            categorized = true;
                            break;
                        }
                    }
                }
            }
        }

        if !categorized {
            for (key, val) in obj {
                if let Some(s) = val.as_str() {
                    if s.len() > 2
                        && s.len() < 30
                        && !["id", "name", "path", "url", "href", "email"].contains(&key.as_str())
                        && !url_re().is_match(s)
                    {
                        let clean_val = s.replace('\n', " ").replace('\r', "").trim().to_string();
                        let entry = categories
                            .entry(format!("{}={}", key, clean_val))
                            .or_insert(0);
                        *entry += 1;
                        break;
                    }
                }
            }
        }
    }

    categories
}

fn find_notable_items(items: &[&Value], max_notable: usize) -> Vec<String> {
    let mut notable = Vec::new();
    let re = notable_re();

    for item in items {
        let item_str = item.to_string();
        let truncated: String = item_str.chars().take(500).collect();
        let matches: Vec<&str> = re
            .find_iter(truncated.as_str())
            .map(|m| m.as_str())
            .collect();
        if !matches.is_empty() {
            let name = item
                .get("name")
                .or_else(|| item.get("id"))
                .or_else(|| item.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if !name.is_empty() {
                let clean_name = name.replace('\n', " ");
                let clean_name = clean_name.trim();
                let truncated_name: String = clean_name.chars().take(50).collect();
                notable.push(format!("{} ({})", truncated_name, matches[0]));
            } else {
                notable.push(matches[0].to_string());
            }

            if notable.len() >= max_notable {
                break;
            }
        }
    }

    notable
}

fn common_keys(items: &[&Value]) -> Vec<String> {
    let mut key_counts: HashMap<String, usize> = HashMap::new();
    for item in items.iter().take(50) {
        if let Some(obj) = item.as_object() {
            for key in obj.keys() {
                *key_counts.entry(key.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut sorted: Vec<(String, usize)> = key_counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    sorted.into_iter().take(8).map(|(k, _)| k).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── TestSummarizeDroppedItems ────────────────────────────────────────

    #[test]
    fn items_with_status_field() {
        let mut all_items: Vec<Value> = (0..50)
            .map(|i| json!({"id": i, "name": format!("item-{}", i), "status": "active"}))
            .collect();
        all_items.extend(
            (0..5).map(|i| json!({"id": i, "name": format!("err-{}", i), "status": "error"})),
        );
        let kept: Vec<Value> = all_items[..3].to_vec();
        let kept_indices: std::collections::HashSet<usize> = (0..3).collect();
        let summary = summarize_dropped_items(&all_items, &kept, Some(&kept_indices), 5, 3);
        assert!(summary.contains("active"));
        assert!(!summary.is_empty());
    }

    #[test]
    fn items_with_type_field() {
        let mut all_items: Vec<Value> = (0..30)
            .map(|i| json!({"type": "log", "message": format!("entry {}", i)}))
            .collect();
        all_items.extend((0..20).map(|i| json!({"type": "metric", "value": i})));
        let kept: Vec<Value> = all_items[..5].to_vec();
        let kept_indices: std::collections::HashSet<usize> = (0..5).collect();
        let summary = summarize_dropped_items(&all_items, &kept, Some(&kept_indices), 5, 3);
        assert!(summary.contains("log") || summary.contains("metric"));
    }

    #[test]
    fn notable_items_with_errors() {
        let mut all_items: Vec<Value> = (0..40)
            .map(|i| json!({"name": format!("test-{}", i), "result": "pass"}))
            .collect();
        all_items
            .push(json!({"name": "test-auth", "result": "fail", "error": "authentication failed"}));
        let kept: Vec<Value> = all_items[..5].to_vec();
        let kept_indices: std::collections::HashSet<usize> = (0..5).collect();
        let summary = summarize_dropped_items(&all_items, &kept, Some(&kept_indices), 5, 3);
        assert!(!summary.is_empty());
    }

    #[test]
    fn no_dropped_items() {
        let items = vec![json!({"id": 1}), json!({"id": 2})];
        let summary = summarize_dropped_items(&items, &items, None, 5, 3);
        assert_eq!(summary, "");
    }

    #[test]
    fn empty_input() {
        let summary = summarize_dropped_items(&[], &[], None, 5, 3);
        assert_eq!(summary, "");
    }

    #[test]
    fn all_items_dropped() {
        let items: Vec<Value> = (0..20)
            .map(|i| json!({"status": "active", "name": format!("item-{}", i)}))
            .collect();
        let summary =
            summarize_dropped_items(&items, &[], Some(&std::collections::HashSet::new()), 5, 3);
        assert!(summary.contains("active"));
    }

    #[test]
    fn mixed_category_fields() {
        let mut all_items: Vec<Value> = (0..10)
            .map(|_| json!({"level": "info", "msg": "something"}))
            .collect();
        all_items.extend((0..3).map(|_| json!({"level": "error", "msg": "bad thing"})));
        let kept: Vec<Value> = all_items[..2].to_vec();
        let kept_indices: std::collections::HashSet<usize> = (0..2).collect();
        let summary = summarize_dropped_items(&all_items, &kept, Some(&kept_indices), 5, 3);
        assert!(!summary.is_empty());
    }

    #[test]
    fn items_without_category_fields() {
        let all_items: Vec<Value> = (0..30).map(|i| json!({"code": 200, "count": i})).collect();
        let kept: Vec<Value> = all_items[..3].to_vec();
        let kept_indices: std::collections::HashSet<usize> = (0..3).collect();
        let summary = summarize_dropped_items(&all_items, &kept, Some(&kept_indices), 5, 3);
        assert!(!summary.is_empty());
    }

    #[test]
    fn summary_not_too_long() {
        let all_items: Vec<Value> = (0..100)
            .map(|i| json!({"type": format!("type_{}", i % 20), "data": "x"}))
            .collect();
        let kept: Vec<Value> = all_items[..5].to_vec();
        let kept_indices: std::collections::HashSet<usize> = (0..5).collect();
        let summary = summarize_dropped_items(&all_items, &kept, Some(&kept_indices), 5, 3);
        assert!(summary.len() < 500);
    }

    #[test]
    fn url_values_excluded_from_categories() {
        let all_items: Vec<Value> = (0..30)
            .map(|i| json!({"url": format!("https://api.example.com/v1/items/{}", i), "method": "GET"}))
            .collect();
        let kept: Vec<Value> = all_items[..3].to_vec();
        let kept_indices: std::collections::HashSet<usize> = (0..3).collect();
        let summary = summarize_dropped_items(&all_items, &kept, Some(&kept_indices), 5, 3);
        assert!(!summary.contains("https://"));
    }

    #[test]
    fn fallback_without_kept_indices() {
        let all_items: Vec<Value> = (0..20)
            .map(|i| json!({"status": "active", "id": i}))
            .collect();
        let kept = vec![all_items[0].clone(), all_items[1].clone()];
        let summary = summarize_dropped_items(&all_items, &kept, None, 5, 3);
        assert!(!summary.is_empty());
    }

    // ── TestSummarizeCompressedCode ──────────────────────────────────────

    #[test]
    fn python_function_bodies() {
        let bodies = vec![
            (
                "def authenticate(username, password):".to_string(),
                "    db = get_db()\n    return True".to_string(),
                10,
            ),
            (
                "def validate_token(token):".to_string(),
                "    return jwt.decode(token)".to_string(),
                20,
            ),
            (
                "def refresh_session(user):".to_string(),
                "    session.extend()".to_string(),
                30,
            ),
        ];
        let summary = summarize_compressed_code(&bodies, 3);
        assert!(summary.contains("3 bodies compressed"));
        assert!(summary.contains("authenticate()"));
        assert!(summary.contains("validate_token()"));
    }

    #[test]
    fn javascript_function_bodies() {
        let bodies = vec![
            (
                "function handleRequest(req, res) {".to_string(),
                "  res.send('ok');".to_string(),
                5,
            ),
            (
                "async function fetchData(url) {".to_string(),
                "  return await fetch(url);".to_string(),
                15,
            ),
        ];
        let summary = summarize_compressed_code(&bodies, 2);
        assert!(summary.contains("handleRequest()"));
        assert!(summary.contains("fetchData()"));
    }

    #[test]
    fn go_function_bodies() {
        let bodies = vec![
            (
                "func (s *Server) HandleRequest(w http.ResponseWriter, r *http.Request) {"
                    .to_string(),
                "  w.Write([]byte(\"ok\"))".to_string(),
                10,
            ),
            (
                "func main() {".to_string(),
                "  server.Start()".to_string(),
                1,
            ),
        ];
        let summary = summarize_compressed_code(&bodies, 2);
        assert!(summary.contains("HandleRequest()"));
        assert!(summary.contains("main()"));
    }

    #[test]
    fn rust_function_bodies() {
        let bodies = vec![(
            "fn authenticate(token: &str) -> Result<User, Error> {".to_string(),
            "  Ok(User::new())".to_string(),
            10,
        )];
        let summary = summarize_compressed_code(&bodies, 1);
        assert!(summary.contains("authenticate()"));
    }

    #[test]
    fn empty_bodies() {
        let summary = summarize_compressed_code(&[], 0);
        assert_eq!(summary, "");
    }

    #[test]
    fn many_bodies_truncated() {
        let bodies: Vec<(String, String, usize)> = (0..20)
            .map(|i| {
                (
                    format!("def func_{}(x):", i),
                    format!("    return {}", i),
                    i * 10,
                )
            })
            .collect();
        let summary = summarize_compressed_code(&bodies, 20);
        assert!(summary.contains("+14 more"));
    }

    // ── TestExtractNameFromSignature ─────────────────────────────────────

    #[test]
    fn python_def() {
        assert_eq!(
            extract_name_from_signature("def authenticate(username):").unwrap(),
            "authenticate()"
        );
    }

    #[test]
    fn python_async_def() {
        assert_eq!(
            extract_name_from_signature("async def fetch_data(url):").unwrap(),
            "fetch_data()"
        );
    }

    #[test]
    fn javascript_function() {
        assert_eq!(
            extract_name_from_signature("function handleClick(event) {").unwrap(),
            "handleClick()"
        );
    }

    #[test]
    fn go_func() {
        assert_eq!(
            extract_name_from_signature("func HandleRequest(w http.ResponseWriter) {").unwrap(),
            "HandleRequest()"
        );
    }

    #[test]
    fn go_method() {
        assert_eq!(
            extract_name_from_signature("func (s *Server) Start() {").unwrap(),
            "Start()"
        );
    }

    #[test]
    fn rust_fn() {
        assert_eq!(
            extract_name_from_signature("fn authenticate(token: &str) -> Result<User> {").unwrap(),
            "authenticate()"
        );
    }

    #[test]
    fn java_method() {
        assert_eq!(
            extract_name_from_signature("public void processPayment(Payment p) {").unwrap(),
            "processPayment()"
        );
    }

    #[test]
    fn class_pattern() {
        assert_eq!(
            extract_name_from_signature("class TokenValidator:").unwrap(),
            "TokenValidator"
        );
    }

    #[test]
    fn empty_signature() {
        assert!(extract_name_from_signature("").is_none());
    }

    #[test]
    fn export_async() {
        assert_eq!(
            extract_name_from_signature("export async function fetchUsers() {").unwrap(),
            "fetchUsers()"
        );
    }
}
