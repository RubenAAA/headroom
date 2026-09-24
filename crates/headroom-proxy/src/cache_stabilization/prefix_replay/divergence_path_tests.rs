use super::*;
use serde_json::json;

#[test]
fn names_the_field_that_changed() {
    let a = json!({"role": "user", "content": [{"type": "text", "text": "at 10:00"}]});
    let b = json!({"role": "user", "content": [{"type": "text", "text": "at 10:05"}]});
    assert_eq!(
        first_structural_difference(&a, &b),
        Some("content[0].text".to_string())
    );
}

/// The whole point of the field: it must be safe to log. A secret in the
/// value must not appear in the path.
#[test]
fn never_reveals_a_value() {
    let a = json!({"content": [{"text": "sk-ant-SECRET-TOKEN-abc123"}]});
    let b = json!({"content": [{"text": "sk-ant-DIFFERENT-xyz789"}]});
    let path = first_structural_difference(&a, &b).expect("differs");
    assert_eq!(path, "content[0].text");
    assert!(!path.contains("SECRET"), "path leaked a value: {path}");
    assert!(!path.contains("sk-ant"), "path leaked a value: {path}");
}

#[test]
fn reports_a_length_change_without_contents() {
    let a = json!({"content": [{"text": "one"}]});
    let b = json!({"content": [{"text": "one"}, {"text": "two"}]});
    let path = first_structural_difference(&a, &b).expect("differs");
    assert!(path.starts_with("content[len "), "got {path}");
    assert!(!path.contains("two"), "path leaked a value: {path}");
}

#[test]
fn a_key_present_on_only_one_side_is_named() {
    let a = json!({"role": "user"});
    let b = json!({"role": "user", "name": "x"});
    assert_eq!(
        first_structural_difference(&a, &b),
        Some("name".to_string())
    );
}

#[test]
fn identical_values_have_no_difference() {
    let a = json!({"content": [{"text": "same"}]});
    assert_eq!(first_structural_difference(&a, &a.clone()), None);
}

/// End to end through the helper the proxy calls, including the
/// canonicalization step: transport churn must not produce a path.
#[test]
fn describe_ignores_what_the_canonicalizer_strips() {
    let prev = vec![json!({
        "role": "user",
        "content": [{"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}]
    })];
    let cur = vec![json!({"role": "user", "content": [{"type": "text", "text": "a"}]})];
    assert_eq!(describe_divergence(&prev, &cur, 0), None);
}
