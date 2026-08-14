//! Item 16 reproducer: does the compression pipeline edit digits inside tool
//! results?
//!
//! A live read of `cache_recache_observed` records came back with
//! `2026-08-08T23:02:36.174635Z` rendered as `2026-08-08T23:2:36.174635Z` — the
//! minute lost its zero padding — while the date, the microseconds, a hex key
//! containing `01`, and a `22:32:11` with no `0N` group all survived. That is
//! the signature of a parse-then-format round trip somewhere in the transforms.
//!
//! Everything else in the observation document is the proxy mis-counting its
//! own work. This would be the proxy handing a model altered data, so it gets a
//! test that runs the real request path rather than an argument about which
//! transform looks guilty.

use bytes::Bytes;
use headroom_core::auth_mode::AuthMode;
use headroom_proxy::compression::{compress_anthropic_request, Outcome};
use headroom_proxy::config::{CacheControlAutoFrozen, CompressionMode};

/// Log-shaped tool output: many consecutive same-shape lines, which is what
/// puts the log template miner and the log compressor on the hot path. The
/// live payload was exactly this — a script printing one record per line.
fn log_shaped_payload() -> String {
    let mut out = String::new();
    out.push_str("2026-08-08T23:02:36.174635Z  early_messages  wasted_tokens 1965\n");
    out.push_str("2026-08-08T23:04:38.372256Z  early_messages  wasted_tokens 143871\n");
    out.push_str("2026-08-08T22:32:11.010203Z  early_messages  wasted_tokens 22032\n");
    for i in 0..60 {
        out.push_str(&format!(
            "2026-08-08T23:0{}:0{}.174635Z INFO worker-{} processing job {} \
             version 1.09 build v2.08.0 key f4993f01a4bc27b6 id-000042 \
             elapsed 00:00:07 at 01:02:03 exit code 007 port 08080\n",
            i % 10,
            i % 10,
            i,
            1000 + i
        ));
    }
    out
}

fn request_with_tool_result(text: &str) -> Bytes {
    let body = serde_json::json!({
        "model": "claude-sonnet-5",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "run the audit"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_01", "name": "Bash",
                 "input": {"command": "python3 audit.py"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01", "content": text}
            ]},
            {"role": "user", "content": "summarise the timestamps above"}
        ]
    });
    Bytes::from(serde_json::to_vec(&body).expect("serialises"))
}

/// The digits a model reads back must be the digits that went in.
///
/// This asserts on *content*, not on size: compression is free to drop, sample,
/// collapse or offload whole lines. What it may never do is emit a different
/// number in a line it chose to keep.
#[test]
fn compression_never_rewrites_digits_inside_a_tool_result() {
    let payload = log_shaped_payload();
    let body = request_with_tool_result(&payload);

    let outcome = compress_anthropic_request(
        &body,
        CompressionMode::AllMessages,
        CacheControlAutoFrozen::Enabled,
        AuthMode::Payg,
        "digit-integrity-test",
        &[],
        None,
    );

    let (compressed, strategies) = match outcome {
        Outcome::Compressed {
            body,
            strategies_applied,
            ..
        } => (body, strategies_applied),
        other => panic!("reproducer did not exercise compression: {other:?}"),
    };
    assert_eq!(strategies, vec!["search_compressor"]);

    let forwarded: serde_json::Value =
        serde_json::from_slice(&compressed).expect("compressed request stays valid JSON");
    let text = forwarded["messages"][2]["content"][0]["content"]
        .as_str()
        .expect("tool_result content stays a string");

    // SearchCompressor is lossy, so assert only on lines its selection policy
    // promises to retain: first + last for the 23:xx group and the sole 22:xx
    // line. This prevents an omitted line from turning the test into a pass.
    let expected_kept_lines = [
        "2026-08-08T23:02:36.174635Z  early_messages  wasted_tokens 1965",
        "2026-08-08T22:32:11.010203Z  early_messages  wasted_tokens 22032",
        "2026-08-08T23:09:09.174635Z INFO worker-59 processing job 1059 version 1.09 build v2.08.0 key f4993f01a4bc27b6 id-000042 elapsed 00:00:07 at 01:02:03 exit code 007 port 08080",
    ];
    for line in expected_kept_lines {
        assert!(
            text.lines().any(|rendered| rendered == line),
            "selected line changed or disappeared: {line:?}\noutput:\n{text}"
        );
    }

    // The damaged forms the round trip produces. If any of these appears and
    // the intact form does not, a transform re-rendered a parsed integer.
    let damaged = [
        ("2026-08-08T23:2:36", "2026-08-08T23:02:36"),
        ("23:2:36", "23:02:36"),
        ("1:2:3", "01:02:03"),
        ("0:0:7", "00:00:07"),
        ("1.9", "1.09"),
        ("v2.8.0", "v2.08.0"),
        ("id-42", "id-000042"),
        ("exit code 7", "exit code 007"),
        ("port 8080", "port 08080"),
    ];
    for (bad, good) in damaged {
        assert!(
            !text.contains(bad) || text.contains(good),
            "digit mutation: output contains {bad:?} but not the original {good:?}"
        );
    }
}
