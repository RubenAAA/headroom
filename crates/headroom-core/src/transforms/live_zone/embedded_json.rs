//! JSON embedded in a larger text block (a `gh api` dump, an MCP result, a
//! `curl | jq` tail).
//!
//! The detector labels such a block `PlainText`, so SmartCrusher never sees the
//! JSON inside it. This finds the balanced JSON containers at any offset, crushes
//! each one that holds an array of objects, and splices the result back with the
//! surrounding bytes kept exact. Port of
//! `upstream-python/headroom/transforms/recursive_json.py`.
//!
//! - A span that already carries a `<<ccr:` marker is never re-crushed.
//! - Traversal is left to right with no clock or rng, so the same block gives the
//!   same bytes on every turn and the prefix cache stays warm.
//! - A span rewrite is kept only when it is smaller in bytes. The caller's
//!   tokenizer gate then decides the whole block, marker included.
//!
//! Two departures from the Python: a block that is one whole JSON value is not
//! skipped, because here the detector sends such an object to `PlainText` and
//! nothing else would crush it; and the span scan has a work budget, so a block
//! of unclosed brackets cannot make the scan quadratic.

use super::*;

/// Total bytes the span scan may read, as a multiple of the block length.
/// Balanced input reads each byte once; only unclosed openers cost more.
const SCAN_BUDGET_FACTOR: usize = 16;

/// Index just past the balanced JSON container that opens at `start`, honouring
/// string and escape rules. `None` if it never balances. Also returns the bytes
/// read, so the caller can charge them to its budget.
fn match_span(bytes: &[u8], start: usize) -> (Option<usize>, usize) {
    let mut stack: Vec<u8> = Vec::new();
    let (mut in_str, mut esc) = (false, false);
    for (j, &ch) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if ch == b'\\' {
                esc = true;
            } else if ch == b'"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            b'"' => in_str = true,
            b'[' | b'{' => stack.push(ch),
            b']' | b'}' => {
                let open = if ch == b']' { b'[' } else { b'{' };
                if stack.last() != Some(&open) {
                    return (None, j - start + 1);
                }
                stack.pop();
                if stack.is_empty() {
                    return (Some(j + 1), j - start + 1);
                }
            }
            _ => {}
        }
    }
    (None, bytes.len() - start)
}

/// `(start, end)` of each top-level balanced JSON container, left to right.
/// Nested containers are not listed on their own; the crusher handles depth.
/// Every delimiter is ASCII, so the byte offsets are always char boundaries.
fn spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut budget = bytes.len().saturating_mul(SCAN_BUDGET_FACTOR);
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b'[' | b'{') {
            let (end, read) = match_span(bytes, i);
            budget = budget.saturating_sub(read);
            if let Some(end) = end {
                out.push((i, end));
                i = end;
                continue;
            }
            if budget == 0 {
                break;
            }
        }
        i += 1;
    }
    out
}

/// True if `span` parses and holds an array of objects somewhere: at least two
/// elements, at least 80% of them objects. The shape SmartCrusher acts on.
fn has_routable_json(span: &str) -> bool {
    fn walk(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Array(items) => {
                let objects = items.iter().filter(|e| e.is_object()).count();
                (items.len() >= 2 && objects * 5 >= items.len() * 4) || items.iter().any(walk)
            }
            serde_json::Value::Object(map) => map.values().any(walk),
            _ => false,
        }
    }
    serde_json::from_str::<serde_json::Value>(span).is_ok_and(|v| walk(&v))
}

/// Crush every embedded JSON span in `text` and splice the results back in
/// place. `None` when no span shrank.
pub(super) fn route_embedded_json(text: &str) -> Option<String> {
    let mut repls: Vec<(usize, usize, String)> = Vec::new();
    for (a, b) in spans(text) {
        let chunk = &text[a..b];
        if chunk.contains("<<ccr:") || !has_routable_json(chunk) {
            continue;
        }
        let crushed = smart_crusher().crush(chunk, EMPTY_QUERY, DEFAULT_BIAS);
        if crushed.was_modified && crushed.compressed.len() < chunk.len() {
            repls.push((a, b, crushed.compressed));
        }
    }
    if repls.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (a, b, replacement) in &repls {
        out.push_str(&text[last..*a]);
        out.push_str(replacement);
        last = *b;
    }
    out.push_str(&text[last..]);
    (out.len() < text.len()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issues(n: usize) -> String {
        let items: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    r#"{{"id":{i},"name":"item-{i}","status":"open","labels":["a","b"],"url":"https://example.com/items/{i}"}}"#
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    #[test]
    fn match_span_honours_strings_and_escapes() {
        let t = r#"{"a":"}]\"}","b":[1,2]} tail"#;
        let (end, _) = match_span(t.as_bytes(), 0);
        assert_eq!(&t[..end.unwrap()], r#"{"a":"}]\"}","b":[1,2]}"#);
    }

    #[test]
    fn match_span_rejects_mismatch_and_unclosed() {
        assert_eq!(match_span(b"[1,2}", 0).0, None);
        assert_eq!(match_span(b"{\"a\":[1,2]", 0).0, None);
    }

    #[test]
    fn spans_are_top_level_only_and_skip_unbalanced() {
        let t = "x [1,[2]] y {\"k\":1} z [oops";
        let found: Vec<&str> = spans(t).into_iter().map(|(a, b)| &t[a..b]).collect();
        assert_eq!(found, vec!["[1,[2]]", "{\"k\":1}"]);
    }

    #[test]
    fn spans_are_char_boundaries_on_multibyte_text() {
        let t = "héllo → [{\"k\":\"ü\"}] ✓";
        for (a, b) in spans(t) {
            assert!(t.is_char_boundary(a) && t.is_char_boundary(b));
        }
    }

    #[test]
    fn unclosed_openers_stay_within_the_scan_budget() {
        // Would be ~n^2/2 reads unbudgeted; a 200k block must finish at once.
        let t = "[".repeat(200_000);
        let start = std::time::Instant::now();
        assert!(spans(&t).is_empty());
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn routable_needs_an_array_of_objects() {
        assert!(has_routable_json(&issues(5)));
        assert!(has_routable_json(&format!(
            r#"{{"total":5,"items":{}}}"#,
            issues(5)
        )));
        assert!(!has_routable_json("[1,2,3,4]"));
        assert!(!has_routable_json(r#"[{"a":1}]"#));
        assert!(!has_routable_json("[not json"));
    }

    #[test]
    fn crushes_the_span_and_keeps_surrounding_bytes_exact() {
        let head = "$ gh api repos/x/y/issues\n";
        let tail = "\n-- exit 0 --\n";
        let block = format!("{head}{}{tail}", issues(60));
        let out = route_embedded_json(&block).expect("span shrinks");
        assert!(out.starts_with(head) && out.ends_with(tail));
        assert!(out.len() < block.len());
    }

    #[test]
    fn same_input_gives_same_bytes() {
        let block = format!("Found:\n{}\nDone.", issues(60));
        assert_eq!(route_embedded_json(&block), route_embedded_json(&block));
    }

    #[test]
    fn a_whole_json_object_is_crushed_too() {
        let block = format!(r#"{{"total":60,"items":{}}}"#, issues(60));
        let out = route_embedded_json(&block).expect("object wrapping an array shrinks");
        assert!(out.len() < block.len());
    }

    #[test]
    fn a_span_with_a_ccr_marker_is_left_alone() {
        let block = format!("{}\n<<ccr:abc123>>", issues(60));
        // The marker sits outside the span here, so the span still crushes ...
        assert!(route_embedded_json(&block).is_some());
        // ... but one carrying its own marker is skipped.
        let inside = format!(r#"{{"note":"<<ccr:abc>>","rows":{}}}"#, issues(60));
        assert_eq!(route_embedded_json(&inside), None);
    }

    #[test]
    fn nothing_routable_returns_none() {
        assert_eq!(
            route_embedded_json("plain prose with [brackets] only"),
            None
        );
        assert_eq!(route_embedded_json(&format!("ids: {}", "[1,2,3]")), None);
    }
}
