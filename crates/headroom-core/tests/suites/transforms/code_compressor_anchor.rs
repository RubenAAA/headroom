//! Code-aware ladder rungs 2–3: roundtrip + anchor-fidelity.
//!
//! Rung 2: a compressed SourceCode block must be resolvable via the
//! dispatcher-level `<<ccr:HASH>>` marker (whole-block granularity — the
//! per-function `[N lines omitted]` comments carry no hash by design).
//! Rung 3: every non-comment skeleton line must anchor verbatim, in order,
//! against the original — the property an `Edit(old_string=)` copied from
//! the skeleton depends on.

use headroom_core::ccr::backends::InMemoryCcrStore;
use headroom_core::ccr::{compute_key, CcrStore};
use headroom_core::transforms::code_compressor::{CodeAwareCompressor, CodeCompressorConfig};
use headroom_core::transforms::live_zone::{
    compress_anthropic_live_zone_with_ccr, AuthMode, DispatchConfig, LiveZoneOutcome, DEFAULT_MODEL,
};
use serde_json::json;

/// Python module sized past the 512B block floor and the 100-token
/// compressor floor, with bodies long enough to trigger elision
/// (`max_body_lines = 5` default).
fn python_module() -> String {
    let mut src = String::from("import os\nimport sys\n\n");
    for f in 0..4 {
        src.push_str(&format!("def worker_{f}(items, config=None):\n"));
        src.push_str(&format!("    \"\"\"Process items for worker {f}.\"\"\"\n"));
        for i in 0..12 {
            src.push_str(&format!(
                "    step_{i} = transform_{f}(items[{i}], config, debug=True, retries=3)\n"
            ));
        }
        src.push_str(&format!("    return aggregate_{f}(step_0, step_11)\n\n"));
    }
    src.push_str("if __name__ == \"__main__\":\n    main(sys.argv)\n");
    assert!(src.len() > 512, "payload must clear the block floor");
    src
}

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

fn is_omission_comment(line: &str) -> bool {
    line.contains("lines omitted")
}

#[test]
fn unit_syntax_valid_and_ratio_floor() {
    let src = python_module();
    let result = CodeAwareCompressor::new(CodeCompressorConfig::default()).compress(&src);
    assert!(result.syntax_valid, "compressed output must re-parse");
    assert!(
        result.compression_ratio < 1.0,
        "expected real compression, ratio={}",
        result.compression_ratio
    );
    assert!(
        result.compression_ratio >= 0.05,
        "ratio guard must hold, ratio={}",
        result.compression_ratio
    );
    assert!(
        result.compressed.contains("lines omitted"),
        "bodies long enough that elision must trigger"
    );
}

#[test]
fn skeleton_lines_anchor_to_original_in_order() {
    let src = python_module();
    let result = CodeAwareCompressor::new(CodeCompressorConfig::default()).compress(&src);
    assert!(result.syntax_valid);

    let orig_lines: Vec<&str> = src.lines().collect();
    let mut cursor = 0usize;
    let mut anchored = 0usize;
    let skel: Vec<&str> = result.compressed.lines().collect();
    for (i, line) in skel.iter().enumerate() {
        if is_omission_comment(line) {
            continue;
        }
        // Validity shim for colon blocks (`{indent}pass` after an omission
        // comment) is the only line the compressor is allowed to invent.
        if line.trim() == "pass"
            && i > 0
            && is_omission_comment(skel[i - 1])
            && !orig_lines.contains(line)
        {
            continue;
        }
        match orig_lines.iter().skip(cursor).position(|l| l == line) {
            Some(pos) => {
                cursor += pos + 1;
                anchored += 1;
            }
            None => panic!("skeleton line anchors nowhere in original: {line:?}"),
        }
    }
    assert!(anchored > 10, "must anchor many lines, got {anchored}");
}

#[test]
fn dispatcher_ccr_roundtrip_for_code_block() {
    let code = python_module();
    let body = body_with_code(&code);
    let store = InMemoryCcrStore::new();

    let outcome = compress_anthropic_live_zone_with_ccr(
        &body,
        0,
        AuthMode::Payg,
        DEFAULT_MODEL,
        Some(&store),
        &DispatchConfig::default(),
    )
    .expect("dispatcher must succeed");

    let new_body = match &outcome {
        LiveZoneOutcome::Modified { new_body, .. } => new_body.get().to_string(),
        LiveZoneOutcome::NoChange { .. } => {
            panic!("expected Modified; code_aware should compress this payload")
        }
    };

    let expected_hash = compute_key(code.as_bytes());
    let marker = format!("<<ccr:{expected_hash}>>");
    assert!(
        new_body.contains(&marker),
        "compressed body must carry the whole-block CCR marker"
    );
    assert_eq!(
        store.get(&expected_hash).as_deref(),
        Some(code.as_str()),
        "omitted bodies recoverable via whole-block retrieve; no dangling marker"
    );
}
