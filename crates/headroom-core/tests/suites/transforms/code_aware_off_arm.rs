//! Off-arm test for the CodeAware process-wide gate.
//!
//! Lives in its own test target (own process) on purpose: the gate is a
//! process-wide `AtomicBool`, so toggling it here cannot race sibling tests
//! the way it would inside a shared test binary.

use headroom_core::transforms::live_zone::{
    code_aware_enabled, compress_anthropic_live_zone, set_code_aware_enabled, AuthMode,
    LiveZoneOutcome, DEFAULT_MODEL,
};
use serde_json::json;

fn body_with_code(code: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "claude-sonnet-5",
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": code}
                ]
            }
        ]
    }))
    .unwrap()
}

fn big_python() -> String {
    let mut src = String::from("import os\n\n");
    for f in 0..4 {
        src.push_str(&format!("def worker_{f}(items):\n"));
        for i in 0..12 {
            src.push_str(&format!("    step_{i} = process_{f}(items[{i}])\n"));
        }
        src.push_str("    return step_11\n\n");
    }
    assert!(src.len() > 512);
    src
}

#[test]
fn gate_defaults_on_and_off_arm_passes_through() {
    // Default: historical behavior — SourceCode compresses.
    assert!(code_aware_enabled(), "gate must default ON");
    let code = big_python();
    let body = body_with_code(&code);
    match compress_anthropic_live_zone(&body, 0, AuthMode::Payg, DEFAULT_MODEL)
        .expect("dispatcher must succeed")
    {
        LiveZoneOutcome::Modified { .. } => {}
        LiveZoneOutcome::NoChange { .. } => {
            panic!("expected Modified with the gate on")
        }
    }

    // Off-arm: the only compressible block is source, so dispatch must
    // report NoChange and the proxy forwards the original bytes.
    set_code_aware_enabled(false);
    assert!(!code_aware_enabled());
    match compress_anthropic_live_zone(&body, 0, AuthMode::Payg, DEFAULT_MODEL)
        .expect("dispatcher must succeed")
    {
        LiveZoneOutcome::NoChange { .. } => {}
        LiveZoneOutcome::Modified { new_body, .. } => {
            assert!(
                new_body.get().contains(&code),
                "off-arm must forward source byte-identical"
            );
        }
    }

    // Restore for process hygiene (nothing else shares this process, but
    // leave the static as found).
    set_code_aware_enabled(true);
}
