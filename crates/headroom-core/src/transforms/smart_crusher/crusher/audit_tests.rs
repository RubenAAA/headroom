//! Audit-safe mode tests (#1705). Fork-only, kept out of `crusher.rs` so
//! that file stays under its size pin and its inline tests match upstream's.

use super::*;
use crate::transforms::anchor_selector::python_safe_json_dumps;
use serde_json::json;

/// Build a large array where two rows carry an `AUDIT_FLAG` note that
/// statistical row-selection would otherwise be free to drop.
fn audit_items() -> Vec<Value> {
    let background: Vec<Value> = (0..60).map(|i| json!({"id": i, "status": "ok"})).collect();
    let protected = [
        json!({"id": 25, "status": "ok", "note": "AUDIT_FLAG: rare compliance event A"}),
        json!({"id": 35, "status": "ok", "note": "AUDIT_FLAG: rare compliance event B"}),
    ];
    let mut items = Vec::new();
    items.extend_from_slice(&background[..25]);
    items.push(protected[0].clone());
    items.extend_from_slice(&background[25..35]);
    items.push(protected[1].clone());
    items.extend_from_slice(&background[35..]);
    items
}

#[test]
fn audit_safe_disabled_by_default_no_behavior_change() {
    // `protected_patterns` set but `audit_safe` left at its false
    // default → byte-identical output to a crusher with no audit-safe
    // config at all. The flag gates the whole feature.
    let items = audit_items();
    let content = python_safe_json_dumps(&Value::Array(items));

    let baseline = SmartCrusher::without_compaction(SmartCrusherConfig::default());
    let configured_but_off = SmartCrusher::without_compaction(SmartCrusherConfig {
        protected_patterns: vec!["AUDIT_FLAG".to_string()],
        ..Default::default()
    });

    let r1 = baseline.smart_crush_content(&content, "", 1.0);
    let r2 = configured_but_off.smart_crush_content(&content, "", 1.0);
    assert_eq!(r1, r2);
}

#[test]
fn audit_safe_with_no_patterns_is_a_no_op() {
    let items = audit_items();
    let content = python_safe_json_dumps(&Value::Array(items));

    let baseline = SmartCrusher::without_compaction(SmartCrusherConfig::default());
    let audit_safe_no_patterns = SmartCrusher::without_compaction(SmartCrusherConfig {
        audit_safe: true,
        ..Default::default()
    });

    assert_eq!(
        baseline.smart_crush_content(&content, "", 1.0),
        audit_safe_no_patterns.smart_crush_content(&content, "", 1.0)
    );
}

#[test]
fn audit_safe_preserves_protected_rows_end_to_end() {
    let items = audit_items();
    let protected_rows = [items[25].clone(), items[36].clone()];
    let content = python_safe_json_dumps(&Value::Array(items.clone()));

    let c = SmartCrusher::without_compaction(SmartCrusherConfig {
        audit_safe: true,
        protected_patterns: vec!["AUDIT_FLAG".to_string()],
        ..Default::default()
    });
    let (crushed, was_modified, _info) = c.smart_crush_content(&content, "", 1.0);
    assert!(was_modified, "input should have compressed");
    let kept: Vec<Value> = serde_json::from_str(&crushed).unwrap();
    for row in &protected_rows {
        assert!(
            kept.contains(row),
            "protected row missing from compressed output: {row:?}"
        );
    }
    // Still compressed overall — protection isn't a blanket opt-out.
    assert!(kept.len() < items.len());
}

#[test]
fn audit_safe_fails_closed_on_non_array_loss() {
    // Non-array crushed output (e.g. a lossless CSV render) that
    // dropped the protected pattern → verification counts a match
    // shortfall and, with the default fail_closed=true, returns the
    // original content unmodified with the fail-closed sentinel.
    let c = SmartCrusher::without_compaction(SmartCrusherConfig {
        audit_safe: true,
        protected_patterns: vec!["AUDIT_FLAG".to_string()],
        ..Default::default()
    });
    let protected = vec![json!({"id": 1, "note": "AUDIT_FLAG"})];
    let original = python_safe_json_dumps(&Value::Array(vec![
        protected[0].clone(),
        json!({"id": 2, "note": "fine"}),
    ]));
    let crushed = "id,note\n2,fine".to_string();

    let (out, was_modified, info) = c.apply_audit_safe_protection_to_content(
        &protected,
        &original,
        crushed,
        true,
        "lossless:table".to_string(),
    );
    assert_eq!(out, original);
    assert!(!was_modified);
    assert_eq!(info, "audit_safe:fail_closed");
}

#[test]
fn audit_safe_ships_best_effort_when_fail_closed_disabled() {
    let c = SmartCrusher::without_compaction(SmartCrusherConfig {
        audit_safe: true,
        protected_patterns: vec!["AUDIT_FLAG".to_string()],
        fail_closed_on_protected_loss: false,
        ..Default::default()
    });
    let protected = vec![json!({"id": 1, "note": "AUDIT_FLAG"})];
    let original = python_safe_json_dumps(&Value::Array(vec![
        protected[0].clone(),
        json!({"id": 2, "note": "fine"}),
    ]));
    let crushed = "id,note\n2,fine".to_string();

    let (out, was_modified, info) = c.apply_audit_safe_protection_to_content(
        &protected,
        &original,
        crushed.clone(),
        true,
        "lossless:table".to_string(),
    );
    // Best-effort: ship the crushed result despite the shortfall.
    assert_eq!(out, crushed);
    assert!(was_modified);
    assert_ne!(info, "audit_safe:fail_closed");
}

#[test]
fn audit_safe_non_array_no_op_when_pattern_count_preserved() {
    let c = SmartCrusher::without_compaction(SmartCrusherConfig {
        audit_safe: true,
        protected_patterns: vec!["AUDIT_FLAG".to_string()],
        ..Default::default()
    });
    let protected = vec![json!({"id": 1, "note": "AUDIT_FLAG"})];
    let original = python_safe_json_dumps(&Value::Array(protected.clone()));
    let crushed = "id,note\n1,AUDIT_FLAG".to_string();

    let (out, was_modified, info) = c.apply_audit_safe_protection_to_content(
        &protected,
        &original,
        crushed.clone(),
        true,
        "lossless:table".to_string(),
    );
    assert_eq!(out, crushed);
    assert!(was_modified);
    assert_eq!(info, "lossless:table");
}

#[test]
#[should_panic(expected = "invalid protected_patterns regex")]
fn invalid_protected_pattern_panics_at_construction() {
    let _ = SmartCrusher::without_compaction(SmartCrusherConfig {
        audit_safe: true,
        protected_patterns: vec!["(".to_string()],
        ..Default::default()
    });
}
