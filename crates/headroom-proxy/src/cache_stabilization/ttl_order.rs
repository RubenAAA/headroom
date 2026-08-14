//! Anthropic's `cache_control` TTL ordering rule, enforced before forwarding.
//!
//! Anthropic reads cache breakpoints in one walk — `tools`, then `system`, then
//! `messages` — and refuses a body where a `ttl: "1h"` marker sits behind a
//! 5-minute one:
//!
//! ```text
//! messages.15.content.1.cache_control.ttl: a ttl='1h' cache_control block must
//! not come after a ttl='5m' cache_control block.
//! ```
//!
//! A marker with no `ttl` key is in the 5-minute lane; that is the default and
//! what an ordinary Claude Code request sends.
//!
//! [`crate::cache_stabilization::cache_ttl`] and the ordering walk in
//! `headroom_core::cache_control` both look at one field list at a time, so a
//! violation that straddles `tools` and `messages` is invisible to them and the
//! second only warns. This module reads the three lists as the single sequence
//! Anthropic reads, and repairs rather than warns: by the time a body reaches
//! the forwarder, a violation in it is one the proxy introduced.
//!
//! Nothing here invents a TTL out of nothing. Every marker the proxy re-places
//! is copied from a marker the client sent, so a 1h marker on a turn whose
//! client sent none was replayed in from an earlier turn — and that client has
//! not sent the extended-cache-ttl beta header either, so the leaked `ttl` is
//! dropped rather than spread.
//!
//! Set `HEADROOM_CACHE_CONTROL_TTL_GUARD=0` to switch the repair off.

use serde_json::Value;

const ONE_HOUR: &str = "1h";
const FIVE_MINUTES: &str = "5m";
const GUARD_ENV: &str = "HEADROOM_CACHE_CONTROL_TTL_GUARD";

/// Which TTL lane one marker asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// `ttl: "1h"`.
    Long,
    /// `ttl: "5m"`, or no `ttl` at all — Anthropic's default.
    Short,
    /// A TTL this code does not model. Counted, never rewritten, so a TTL
    /// Anthropic adds later cannot be mangled by guesswork here.
    Other,
}

fn lane(marker: &Value) -> Lane {
    let Some(obj) = marker.as_object() else {
        return Lane::Other;
    };
    match obj.get("ttl") {
        None => Lane::Short,
        Some(Value::String(ttl)) if ttl == ONE_HOUR => Lane::Long,
        Some(Value::String(ttl)) if ttl == FIVE_MINUTES => Lane::Short,
        _ => Lane::Other,
    }
}

/// True when the request asks for the 1h lane anywhere.
pub fn asks_for_1h(body: &Value) -> bool {
    let mut found = false;
    walk_markers(body, &mut |marker| {
        if lane(marker) == Lane::Long {
            found = true;
        }
    });
    found
}

/// What a repair pass changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TtlOrderRepair {
    /// Leaked 1h markers whose `ttl` was dropped.
    pub demoted: usize,
    /// 5m markers ahead of a 1h one that were lifted into the 1h lane.
    pub promoted: usize,
}

impl TtlOrderRepair {
    pub fn is_noop(&self) -> bool {
        self.demoted == 0 && self.promoted == 0
    }
}

fn guard_enabled() -> bool {
    !matches!(
        std::env::var(GUARD_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Make `body` satisfy the ordering rule. `client_asked_for_1h` comes from the
/// client's own request, not from this one — that is the whole point of the
/// first repair.
///
/// Two passes, in this order:
///
/// 1. **Containment.** The client asked for no 1h caching, so every outbound 1h
///    marker leaked in from an earlier turn: drop the `ttl` and leave the
///    marker where it is. Promoting the rest of the body to match a lane the
///    client never asked for is not available — the beta header is theirs to
///    send.
/// 2. **Ordering.** The client is in the 1h lane, so every 5m marker ahead of
///    the last 1h one is lifted to 1h. Demoting the 1h markers instead would
///    also satisfy the rule, and would throw away caching the client asked and
///    paid for.
pub fn enforce_ttl_order(body: &mut Value, client_asked_for_1h: bool) -> TtlOrderRepair {
    let mut repair = TtlOrderRepair::default();
    if !guard_enabled() {
        return repair;
    }

    if !client_asked_for_1h {
        walk_markers_mut(body, &mut |marker| {
            if lane(marker) != Lane::Long {
                return;
            }
            if let Some(obj) = marker.as_object_mut() {
                obj.remove("ttl");
                repair.demoted += 1;
            }
        });
        return repair;
    }

    // Survey first: the promotion only reaches markers ahead of the last 1h
    // one, and that position is not known until the whole body has been read.
    let mut lanes: Vec<Lane> = Vec::new();
    walk_markers(body, &mut |marker| lanes.push(lane(marker)));
    let Some(last_long) = lanes.iter().rposition(|l| *l == Lane::Long) else {
        return repair;
    };

    let mut position = 0usize;
    walk_markers_mut(body, &mut |marker| {
        let index = position;
        position += 1;
        if index >= last_long || lane(marker) != Lane::Short {
            return;
        }
        if let Some(obj) = marker.as_object_mut() {
            obj.insert("ttl".to_string(), Value::String(ONE_HOUR.to_string()));
            repair.promoted += 1;
        }
    });
    repair
}

/// Visit every `cache_control` marker in the order Anthropic reads them:
/// `tools`, then `system`, then `messages` — message level, block level, and
/// the blocks nested inside a `tool_result`.
fn walk_markers(body: &Value, visit: &mut impl FnMut(&Value)) {
    fn at(holder: &Value, visit: &mut impl FnMut(&Value)) {
        if let Some(marker) = holder.get("cache_control") {
            visit(marker);
        }
    }
    fn array<'a>(holder: &'a Value, key: &str) -> &'a [Value] {
        holder
            .get(key)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
    for tool in array(body, "tools") {
        at(tool, visit);
    }
    for block in array(body, "system") {
        at(block, visit);
    }
    for message in array(body, "messages") {
        at(message, visit);
        for block in array(message, "content") {
            at(block, visit);
            for sub in array(block, "content") {
                at(sub, visit);
            }
        }
    }
}

/// [`walk_markers`] with the markers handed over for rewriting. The two
/// traversals must stay in step: the ordering pass surveys with one and
/// repairs with the other, keyed by position.
fn walk_markers_mut(body: &mut Value, visit: &mut impl FnMut(&mut Value)) {
    fn at(holder: &mut Value, visit: &mut impl FnMut(&mut Value)) {
        if let Some(marker) = holder.get_mut("cache_control") {
            visit(marker);
        }
    }
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            at(tool, visit);
        }
    }
    if let Some(blocks) = body.get_mut("system").and_then(Value::as_array_mut) {
        for block in blocks {
            at(block, visit);
        }
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            at(message, visit);
            let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for block in blocks {
                at(block, visit);
                if let Some(inner) = block.get_mut("content").and_then(Value::as_array_mut) {
                    for sub in inner {
                        at(sub, visit);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The `/btw` case: a side question in the 5m lane, with a 1h marker
    /// replayed into its history from the session it forked off.
    fn leaked_1h_body() -> Value {
        json!({
            "tools": [{"name": "a", "cache_control": {"type": "ephemeral"}}],
            "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "u"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "a",
                     "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]}
            ]
        })
    }

    #[test]
    fn a_leaked_1h_marker_loses_its_ttl() {
        let mut body = leaked_1h_body();
        let repair = enforce_ttl_order(&mut body, false);
        assert_eq!(repair.demoted, 1);
        assert_eq!(repair.promoted, 0);
        let marker = &body["messages"][1]["content"][0]["cache_control"];
        assert!(marker.get("ttl").is_none(), "the leaked ttl is dropped");
        // The breakpoint itself is the client's placement and stays put.
        assert_eq!(marker["type"], "ephemeral");
    }

    /// The mirror-image bug: the client is in the 1h lane and an earlier
    /// breakpoint was left at 5m, which Anthropic refuses outright.
    #[test]
    fn a_short_marker_ahead_of_a_long_one_is_promoted() {
        let mut body = leaked_1h_body();
        let repair = enforce_ttl_order(&mut body, true);
        assert_eq!(repair.promoted, 2, "tools and system both sit ahead");
        assert_eq!(repair.demoted, 0);
        assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"]["ttl"],
            "1h"
        );
    }

    /// The ordinary request: every marker in the 5m lane, nothing to repair.
    #[test]
    fn an_all_5m_body_is_left_alone() {
        let mut body = json!({
            "tools": [{"name": "a", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "u", "cache_control": {"type": "ephemeral"}}
            ]}]
        });
        let before = body.clone();
        assert!(enforce_ttl_order(&mut body, false).is_noop());
        assert!(enforce_ttl_order(&mut body, true).is_noop());
        assert_eq!(body, before);
    }

    /// Markers behind the last 1h one are already legal.
    #[test]
    fn a_short_marker_behind_the_last_long_one_stays_short() {
        let mut body = json!({
            "tools": [{"name": "a", "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "u", "cache_control": {"type": "ephemeral"}}
            ]}]
        });
        assert!(enforce_ttl_order(&mut body, true).is_noop());
        assert!(body["messages"][0]["content"][0]["cache_control"]
            .get("ttl")
            .is_none());
    }

    /// A marker inside a `tool_result`'s own content counts as a breakpoint,
    /// so the walk has to reach it.
    #[test]
    fn nested_tool_result_markers_are_walked() {
        let mut body = json!({
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "content": [
                    {"type": "text", "text": "r",
                     "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]}
            ]}]
        });
        assert!(asks_for_1h(&body));
        let repair = enforce_ttl_order(&mut body, false);
        assert_eq!(repair.demoted, 1);
    }

    /// A TTL this code does not model is counted and left as it is.
    #[test]
    fn an_unmodelled_ttl_is_not_rewritten() {
        let mut body = json!({
            "tools": [{"name": "a", "cache_control": {"type": "ephemeral", "ttl": "7d"}}],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "u",
                 "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ]}]
        });
        let repair = enforce_ttl_order(&mut body, true);
        assert!(repair.is_noop(), "only 5m markers are promoted");
        assert_eq!(body["tools"][0]["cache_control"]["ttl"], "7d");
    }

    #[test]
    fn asks_for_1h_reads_every_section() {
        assert!(!asks_for_1h(&json!({"messages": []})));
        assert!(asks_for_1h(&json!({
            "system": [{"type": "text", "text": "s",
                        "cache_control": {"type": "ephemeral", "ttl": "1h"}}]
        })));
    }
}
