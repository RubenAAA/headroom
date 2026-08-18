//! Put Claude Code's message breakpoint on the last content block.
//!
//! The client spends three of Anthropic's four `cache_control` markers: two on
//! the `system` array and exactly one on the messages. That one is already on
//! the final content block on 97% of captured requests. On the rest it sits
//! short of the tail, and everything after it is written fresh every turn.
//!
//! Moving it forward is measured at -0.9% of the bill under API weights and
//! -3.5% under subscription weights on a 7,839-turn capture, and at exactly
//! zero on a 1,009-turn capture where the client had already placed it at the
//! tail on 384 of 389 requests. It is a no-op whenever the marker is where it
//! should be, so the cost on traffic that does not need it is nothing.
//!
//! # The fourth breakpoint is not worth spending
//!
//! The obvious other move is to add a second message marker some way back
//! through history, since the client leaves one of the four unused. Swept at
//! seven fractions from 2% to 50% across both corpora, it changed the bill by
//! nothing at all — the arms came out byte-identical to the untouched proxy.
//!
//! Each turn's tail breakpoint becomes the next turn's read point, so on a
//! conversation that only grows, an earlier marker never holds the longest
//! live prefix and never gets read. It would pay only when the history below
//! the tail is edited, and that is rare enough here not to register. The
//! measurement that first suggested otherwise had a TTL change folded into the
//! same arm; separating the two left the breakpoint contributing zero and the
//! tail move carrying all of it.
use serde_json::Value;

/// Move the message breakpoint to the last content block. Returns whether the
/// body changed.
///
/// Does nothing unless the client placed exactly one block-level marker on
/// `messages`. Zero means it is not caching the messages at all and one marker
/// of ours would not change that; two or more is a placement this was never
/// measured against, and guessing at it risks the prefix for no known gain.
pub fn push_marker_to_tail(body: &mut Value) -> bool {
    // Message content that is still a bare string has no block to carry a
    // marker. Converting it would rewrite the message and cost the prefix at
    // the exact point of the edit, so those messages are passed over instead.
    let mut positions: Vec<(usize, usize)> = Vec::new();
    let mut marked: Vec<usize> = Vec::new();
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return false;
    };
    for (m, message) in messages.iter().enumerate() {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for (b, block) in blocks.iter().enumerate() {
            if !block.is_object() {
                continue;
            }
            if block.get("cache_control").is_some() {
                marked.push(positions.len());
            }
            positions.push((m, b));
        }
    }
    if marked.len() != 1 || marked[0] + 1 == positions.len() {
        return false;
    }

    let (fm, fb) = positions[marked[0]];
    let marker = body["messages"][fm]["content"][fb]
        .as_object_mut()
        .expect("marked position is an object")
        .remove("cache_control")
        .expect("marked position carried a marker");
    let (tm, tb) = positions[positions.len() - 1];
    body["messages"][tm]["content"][tb]
        .as_object_mut()
        .expect("collected positions are objects")
        .insert("cache_control".to_string(), marker);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `n` single-block messages with the marker on block `at`.
    fn body_marked_at(n: usize, at: usize) -> Value {
        let mut messages = Vec::new();
        for i in 0..n {
            let mut block = json!({"type": "text", "text": format!("m{i}")});
            if i == at {
                block["cache_control"] = json!({"type": "ephemeral"});
            }
            messages.push(json!({"role": "user", "content": [block]}));
        }
        json!({ "messages": messages })
    }

    fn marked_indices(body: &Value) -> Vec<usize> {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, m)| m["content"][0].get("cache_control").is_some())
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn a_marker_short_of_the_tail_moves_to_it() {
        let mut body = body_marked_at(20, 14);
        assert!(push_marker_to_tail(&mut body));
        assert_eq!(marked_indices(&body), vec![19]);
    }

    /// The common case, and the reason this is cheap: 97% of captured requests
    /// already have the marker where it belongs.
    #[test]
    fn a_marker_already_at_the_tail_is_left_alone() {
        let mut body = body_marked_at(20, 19);
        let before = body.clone();
        assert!(!push_marker_to_tail(&mut body));
        assert_eq!(body, before);
    }

    /// The count must not change. Two markers on the messages plus the two the
    /// client puts on `system` is the limit, and the API refuses a fifth
    /// rather than dropping one.
    #[test]
    fn the_marker_count_never_changes() {
        let mut body = body_marked_at(20, 14);
        push_marker_to_tail(&mut body);
        assert_eq!(marked_indices(&body).len(), 1);
    }

    /// Zero markers and two markers are both placements this was never
    /// measured against.
    #[test]
    fn placements_this_was_not_measured_against_are_left_alone() {
        let mut none = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "a"}]},
            {"role": "user", "content": [{"type": "text", "text": "b"}]},
        ]});
        let before = none.clone();
        assert!(!push_marker_to_tail(&mut none));
        assert_eq!(none, before);

        let mut two = body_marked_at(20, 14);
        two["messages"][0]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
        let before = two.clone();
        assert!(!push_marker_to_tail(&mut two));
        assert_eq!(two, before);
    }

    /// String content carries no block, so it cannot take the marker and must
    /// not be rewritten into one. Here the last message is a bare string, so
    /// the marker stops at the last real block before it.
    #[test]
    fn string_content_is_passed_over_not_converted() {
        let mut body = body_marked_at(20, 14);
        body["messages"][19]["content"] = json!("a bare string");
        assert!(push_marker_to_tail(&mut body));
        assert!(body["messages"][19]["content"].is_string());
        assert_eq!(marked_indices(&body), vec![18]);
    }

    /// The marker is moved, not rewritten, so the TTL the client asked for
    /// rides along and this stage never becomes a TTL change in disguise.
    #[test]
    fn the_markers_own_ttl_survives_the_move() {
        let mut body = body_marked_at(20, 14);
        body["messages"][14]["content"][0]["cache_control"] =
            json!({"type": "ephemeral", "ttl": "1h"});
        assert!(push_marker_to_tail(&mut body));
        assert_eq!(
            body["messages"][19]["content"][0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
    }
}
