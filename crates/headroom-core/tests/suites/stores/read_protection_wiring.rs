//! `HEADROOM_PROTECT_READS` is honored by the dispatchers, not just parsed.
//!
//! The predicates have their own unit tests next to them. What these cover is
//! the wiring: a flag read by nobody would pass every one of those and still
//! let a `cat` of a source file reach a lossy compressor.
//!
//! Every case is driven twice, once with the flag off, so each assertion is a
//! difference the flag made rather than a property the fixture had anyway.

use std::sync::{Mutex, MutexGuard};

use headroom_core::transforms::live_zone::{
    compress_anthropic_live_zone, compress_openai_responses_live_zone, AuthMode, BlockAction,
    ExclusionReason, LiveZoneOutcome,
};
use headroom_core::transforms::read_protection::read_protection_enabled;
use serde_json::json;

const MODEL: &str = "claude-3-5-sonnet-20241022";

/// `set_var` is process-wide, so the cases take turns.
static ENV: Mutex<()> = Mutex::new(());

fn with_flag(on: bool) -> MutexGuard<'static, ()> {
    let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    if on {
        std::env::set_var("HEADROOM_PROTECT_READS", "1");
    } else {
        std::env::remove_var("HEADROOM_PROTECT_READS");
    }
    guard
}

/// The flag drives every path, and `set_var` is process-wide, so its parsing
/// is pinned here rather than beside the function — a unit test toggling it
/// would be toggling it for every other test sharing that process.
#[test]
fn the_flag_is_off_until_it_says_otherwise() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    for (raw, want) in [
        ("", false),
        ("0", false),
        ("false", false),
        ("NO", false),
        ("  0  ", false),
        ("1", true),
        ("true", true),
        ("yes", true),
        ("on", true),
    ] {
        std::env::set_var("HEADROOM_PROTECT_READS", raw);
        assert_eq!(read_protection_enabled(), want, "for {raw:?}");
    }
    std::env::remove_var("HEADROOM_PROTECT_READS");
}

/// Python source, well over the byte threshold, of a kind the detector calls
/// source code — the working set an agent is about to patch.
fn source_payload() -> String {
    let mut s = String::from("import os\nimport sys\n\n\n");
    for i in 0..60 {
        s.push_str(&format!(
            "def compute_overdraft_{i}(business_id: int, amount: float) -> float:\n\
             \x20   \"\"\"Return the overdraft for one business.\"\"\"\n\
             \x20   base = amount * {i}.0\n\
             \x20   if base < 0:\n\
             \x20       raise ValueError(\"negative\")\n\
             \x20   return base + os.environ.get(\"FEE_{i}\", 0)\n\n\n"
        ));
    }
    s
}

/// A JSON array — data, and nobody patches it byte for byte.
fn data_payload() -> String {
    let items: Vec<_> = (0..60)
        .map(|i| {
            json!({
                "id": i,
                "name": format!("entry_{i}"),
                "score": i * 7,
                "notes": "lorem ipsum dolor sit amet, consectetur adipiscing elit",
            })
        })
        .collect();
    serde_json::to_string(&items).unwrap()
}

/// One bash call and the result it produced.
fn anthropic_body(command: &str, payload: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": MODEL,
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash",
                 "input": {"command": command}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": payload},
            ]},
        ]
    }))
    .unwrap()
}

fn anthropic_outcome(command: &str, payload: &str) -> LiveZoneOutcome {
    let body = anthropic_body(command, payload);
    compress_anthropic_live_zone(&body, 0, AuthMode::Payg, MODEL).expect("dispatcher")
}

fn protected_read_count(outcome: &LiveZoneOutcome) -> usize {
    let manifest = match outcome {
        LiveZoneOutcome::Modified { manifest, .. } => manifest,
        LiveZoneOutcome::NoChange { manifest } => manifest,
    };
    manifest
        .block_outcomes
        .iter()
        .filter(|b| {
            matches!(
                b.action,
                BlockAction::Excluded {
                    reason: ExclusionReason::ProtectedRead
                }
            )
        })
        .count()
}

fn changed(outcome: &LiveZoneOutcome) -> bool {
    matches!(outcome, LiveZoneOutcome::Modified { .. })
}

#[test]
fn a_source_read_is_protected_only_while_the_flag_is_on() {
    let payload = source_payload();

    let _g = with_flag(true);
    let on = anthropic_outcome("cat src/overdraft.py", &payload);
    assert_eq!(protected_read_count(&on), 1, "the read must be excluded");
    assert!(!changed(&on), "a protected read must reach the model whole");
    drop(_g);

    let _g = with_flag(false);
    let off = anthropic_outcome("cat src/overdraft.py", &payload);
    assert_eq!(protected_read_count(&off), 0);
    assert!(
        changed(&off),
        "without the flag the same read is compressed — otherwise the test \
         above proves nothing"
    );
}

/// The command gate says "read"; the content gate then releases data back to
/// its own compressor. Both have to run, or every `cat` of a JSON file would
/// be forwarded whole.
#[test]
fn a_read_of_data_is_released_to_its_compressor() {
    let payload = data_payload();
    let _g = with_flag(true);
    let outcome = anthropic_outcome("cat rows.json", &payload);
    assert_eq!(protected_read_count(&outcome), 0);
    assert!(changed(&outcome));
}

/// Derived output describes a file rather than reproducing it, so nothing is
/// patched from it.
#[test]
fn search_output_is_not_a_read() {
    let payload = source_payload();
    let _g = with_flag(true);
    let outcome = anthropic_outcome("grep -rn compute_overdraft src/", &payload);
    assert_eq!(protected_read_count(&outcome), 0);
    assert!(changed(&outcome));
}

/// Lockfiles read as plain text, so the content gate would protect them, and
/// they are the largest repeated read in a session. The command gate has to
/// catch them by name.
#[test]
fn a_lockfile_read_stays_compressible() {
    let payload = source_payload();
    let _g = with_flag(true);
    let outcome = anthropic_outcome("cat package-lock.json", &payload);
    assert_eq!(protected_read_count(&outcome), 0);
    assert!(changed(&outcome));
}

/// Harnesses prefix nearly every command with a `cd` into the checkout, and a
/// first-token match would see `cd` and give up.
#[test]
fn a_cd_prefixed_read_is_still_a_read() {
    let payload = source_payload();
    let _g = with_flag(true);
    let outcome = anthropic_outcome("cd /repo && cat src/overdraft.py", &payload);
    assert_eq!(protected_read_count(&outcome), 1);
}

fn responses_body(item: serde_json::Value, payload: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "gpt-5.4",
        "input": [
            item,
            {"type": "function_call_output", "call_id": "c1", "output": payload},
        ]
    }))
    .unwrap()
}

fn responses_outcome(item: serde_json::Value, payload: &str) -> LiveZoneOutcome {
    let body = responses_body(item, payload);
    compress_openai_responses_live_zone(&body, AuthMode::Payg, "gpt-5.4").expect("dispatcher")
}

/// Codex sends shell calls as `function_call` with JSON-string arguments. The
/// parsed command is what has to be read: the raw argument blob is
/// `{"command": ...}` JSON, which no read predicate ever matches.
#[test]
fn the_responses_path_protects_a_function_call_read() {
    let payload = source_payload();
    let call = json!({
        "type": "function_call", "call_id": "c1", "name": "shell",
        "arguments": r#"{"command": "cat src/overdraft.py"}"#,
    });

    let _g = with_flag(true);
    let on = responses_outcome(call.clone(), &payload);
    assert_eq!(protected_read_count(&on), 1);
    drop(_g);

    let _g = with_flag(false);
    let off = responses_outcome(call, &payload);
    assert_eq!(protected_read_count(&off), 0);
    assert!(changed(&off));
}

/// Codex's native shell tool arrives as `local_shell_call` carrying an
/// `action`, not as a `function_call`. Reading only one shape covers half the
/// harness.
#[test]
fn the_responses_path_protects_a_local_shell_call_read() {
    let payload = source_payload();
    let call = json!({
        "type": "local_shell_call", "call_id": "c1",
        "action": {"type": "exec", "command": ["cat", "src/overdraft.py"]},
    });
    let _g = with_flag(true);
    assert_eq!(protected_read_count(&responses_outcome(call, &payload)), 1);
}

#[test]
fn the_responses_path_protects_a_string_local_shell_call_read() {
    let payload = source_payload();
    let call = json!({
        "type": "local_shell_call", "call_id": "c1", "action": "cat src/overdraft.py",
    });
    let _g = with_flag(true);
    assert_eq!(protected_read_count(&responses_outcome(call, &payload)), 1);
}

/// Copilot's first-class `view` tool is a byte-exact file read even without
/// the shell-command flag. It is part of the built-in verbatim exclusion set.
#[test]
fn the_responses_view_tool_is_never_lossy() {
    let payload = source_payload();
    let call = json!({
        "type": "function_call", "call_id": "c1", "name": "view",
        "arguments": r#"{"path": "src/overdraft.py"}"#,
    });
    let _g = with_flag(false);
    let outcome = responses_outcome(call, &payload);
    assert!(!changed(&outcome));
}
