//! B2 — tool-order stabilization.
//!
//! # Why this exists
//!
//! `tools` sits at the head of Anthropic's prompt-cache key: the provider
//! caches the longest byte-identical prefix of `[tools, system, messages]`, so
//! the *first* tool whose bytes move invalidates every tool after it plus the
//! whole system prompt and the whole message history. On a real 39-turn capture
//! corpus tools carry 46.4% of payload mass, which makes their ordering the
//! single most expensive thing a client can wobble on.
//!
//! Clients wobble on it routinely. An MCP server that finishes its handshake on
//! turn 20 shows up as two extra tool definitions spliced into the middle of the
//! array — measured on the corpus: two `mcp__ai-lens__*` tools landing at index
//! 33 of 97 busted 79.2% of the request (~104k tokens re-created at full price).
//! Nothing about the conversation changed; the client just enumerated its tools
//! in a different order.
//!
//! # What it does
//!
//! Remember the tool-name order we forwarded last turn. On the next turn, emit
//! the tools the provider already has cached in exactly that order and append
//! genuinely-new ones at the end, so the divergence point moves from wherever
//! the client put the new tool to the tail of the array. On the measured event
//! that recovers 20.5k tokens of the 104k bust (19.7% of the busted region).
//!
//! Reordering is **lossless**: the request carries the same set of tool
//! definitions, byte-identical, and the Messages API assigns no meaning to their
//! position.
//!
//! # When it declines to act
//!
//! Two guards, both deliberately conservative — B2's whole premise is that
//! busting the cache costs more than any optimization can win back, so a
//! stabilizer that guesses wrong is worse than no stabilizer.
//!
//! - **Any tool carries a `cache_control` marker.** Moving a tool then moves the
//!   provider's breakpoint, which is exactly the bust we're preventing. This
//!   also keeps us out of PR-E1/PR-E3's way: on PAYG,
//!   [`auto_place_anthropic_cache_control`] has already put a marker on the last
//!   tool by the time we run, so B2 self-disables and the alphabetic sort owns
//!   the ordering. Sorting and replaying disagree — see below — and only one of
//!   them can be right per request.
//!
//!   [`auto_place_anthropic_cache_control`]: super::anthropic_cache_control::auto_place_anthropic_cache_control
//!
//! - **The remembered order is not a subset of the current tool set.** A tool
//!   that *disappeared* leaves a hole that busts at that index no matter what
//!   order we choose, and a wholly different tool set means we're looking at a
//!   different agent sharing one session key (a subagent spawn, say) whose
//!   cached prefix has nothing to do with ours. Replay only helps when this turn
//!   *extends* the last one.
//!
//! # Why not just sort alphabetically
//!
//! PR-E1's [`sort_tools_deterministically`] fixes a different failure: a client
//! whose order is *unstable*, where any fixed order beats none. Applied to a
//! client whose order is already stable it is actively harmful, because sorting
//! relocates the tools relative to what the provider cached. Measured on the
//! same event: client-as-is busts 104,188 tokens, sorted-by-name busts 120,794,
//! replay-and-append busts 83,643. Replay wins because it optimizes against what
//! the provider actually holds rather than against an abstract canonical form.
//!
//! [`sort_tools_deterministically`]: super::tool_def_normalize::sort_tools_deterministically
//!
//! # Keying
//!
//! Per `(session_key, model)`. The model has to be in the key:
//! [`derive_session_key`] falls back to a credential hash when the client sends
//! no `x-headroom-session-id`, so a main agent and its subagents collapse into
//! one session, and on the corpus the subagent's 81 tools are a strict subset of
//! the main agent's 95 — the subset guard alone would happily replay the
//! subagent's order onto the main agent's array.
//!
//! [`derive_session_key`]: super::drift_detector::derive_session_key

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use lru::LruCache;
use serde_json::Value;

/// Production session capacity — matches the drift detector's and the prefix
/// replay store's 1000.
pub const TOOL_ORDER_STORE_CAPACITY: usize = 1000;

/// The name a tool definition is addressed by, in either the Anthropic shape
/// (`{"name": ...}`) or the OpenAI function shape (`{"function": {"name": ...}}`).
fn tool_name(tool: &Value) -> Option<&str> {
    tool.get("name")
        .and_then(Value::as_str)
        .or_else(|| tool.get("function")?.get("name")?.as_str())
}

/// The forwarded order of `tools`, for handing to [`stabilize_tool_order`] on
/// the next turn. Unnamed tools are skipped: they cannot be matched by name, so
/// recording them would make the subset guard fail spuriously.
pub fn tool_order(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|t| tool_name(t).map(str::to_string))
        .collect()
}

/// True when any tool carries a top-level `cache_control` marker.
///
/// Mirrors [`super::tool_def_normalize::any_tool_has_cache_control`]; kept
/// separate so B2's guard does not move if PR-E1's contract changes.
fn any_tool_marked(tools: &[Value]) -> bool {
    tools.iter().any(|t| t.get("cache_control").is_some())
}

/// Reorder `tools` in place to lead with `previous`'s order.
///
/// Returns `true` when the array was reordered. A `false` return leaves `tools`
/// untouched, byte for byte.
///
/// Tools named in `previous` come first, in `previous`'s order; everything else
/// follows in the caller's original relative order. Tools that appear more than
/// once under one name keep their relative order among themselves (the match is
/// first-unclaimed-wins), so a duplicate name cannot drop a definition.
pub fn stabilize_tool_order(tools: &mut Vec<Value>, previous: &[String]) -> bool {
    if previous.is_empty() || tools.len() < 2 {
        return false;
    }
    if any_tool_marked(tools) {
        return false;
    }

    // Subset guard: every remembered tool must still be present, or the
    // provider's cached prefix has a hole in it that reordering cannot close.
    let mut names: Vec<Option<&str>> = tools.iter().map(|t| tool_name(t)).collect();
    for want in previous {
        if !names.iter().any(|n| n.is_some_and(|n| n == want.as_str())) {
            return false;
        }
    }

    // Claim one slot per remembered name, in remembered order. `names[i]` is
    // taken to `None` when claimed so duplicates are consumed one at a time.
    let mut order: Vec<usize> = Vec::with_capacity(tools.len());
    for want in previous {
        if let Some(i) = names
            .iter()
            .position(|n| n.is_some_and(|n| n == want.as_str()))
        {
            names[i] = None;
            order.push(i);
        }
    }
    // Everything unclaimed — genuinely new tools, plus any unnamed ones —
    // follows in the caller's order.
    order.extend((0..tools.len()).filter(|i| names[*i].is_some()));

    if order.iter().copied().eq(0..tools.len()) {
        return false; // already in the remembered order
    }

    let mut slots: Vec<Option<Value>> = tools.drain(..).map(Some).collect();
    for i in order {
        // Every index appears exactly once, so the take always yields.
        if let Some(tool) = slots[i].take() {
            tools.push(tool);
        }
    }
    true
}

/// Per-session record of the tool order last forwarded upstream.
#[derive(Clone)]
pub struct ToolOrderStore {
    inner: Arc<Mutex<LruCache<String, Vec<String>>>>,
}

impl std::fmt::Debug for ToolOrderStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolOrderStore").finish_non_exhaustive()
    }
}

impl Default for ToolOrderStore {
    fn default() -> Self {
        Self::new(TOOL_ORDER_STORE_CAPACITY)
    }
}

impl ToolOrderStore {
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(cap))),
        }
    }

    /// Stabilize `tools` against this session's last forwarded order, then
    /// record the order actually forwarded.
    ///
    /// Returns `true` when the array was reordered. The record is written on
    /// every call — including the first, and including calls the guards
    /// declined — so a declined turn re-anchors on what the provider is about
    /// to cache rather than on a stale order it no longer holds.
    pub fn stabilize(&self, session_key: &str, model: &str, tools: &mut Vec<Value>) -> bool {
        let key = format!("{session_key}\u{1f}{model}");
        let previous = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.get(&key).cloned()
        };

        let reordered = previous
            .map(|prev| stabilize_tool_order(tools, &prev))
            .unwrap_or(false);

        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.put(key, tool_order(tools));
        reordered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools(names: &[&str]) -> Vec<Value> {
        names
            .iter()
            .map(|n| json!({"name": n, "input_schema": {"type": "object"}}))
            .collect()
    }

    fn names(tools: &[Value]) -> Vec<String> {
        tool_order(tools)
    }

    /// The measured failure: an MCP server finishes its handshake mid-session
    /// and the client splices its tools into the middle of the array. B2 has to
    /// push them to the end so the divergence point moves off the cached prefix.
    #[test]
    fn late_mcp_tools_move_to_the_end() {
        let previous = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut current = tools(&["a", "new1", "b", "new2", "c"]);
        assert!(stabilize_tool_order(&mut current, &previous));
        assert_eq!(names(&current), ["a", "b", "c", "new1", "new2"]);
    }

    /// A well-behaved client is already in the remembered order; B2 must not
    /// touch the bytes, or it becomes the churn it exists to prevent.
    #[test]
    fn steady_state_is_a_byte_identical_no_op() {
        let previous = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut current = tools(&["a", "b", "c"]);
        let before = current.clone();
        assert!(!stabilize_tool_order(&mut current, &previous));
        assert_eq!(current, before);
    }

    /// The client shuffled a stable tool set. Replaying the remembered order
    /// restores the bytes the provider has cached.
    #[test]
    fn shuffled_order_is_restored() {
        let previous = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut current = tools(&["c", "a", "b"]);
        assert!(stabilize_tool_order(&mut current, &previous));
        assert_eq!(names(&current), ["a", "b", "c"]);
    }

    /// A tool that disappeared leaves a hole the provider already busted on.
    /// Reordering the survivors cannot close it and risks making things worse,
    /// so decline.
    #[test]
    fn removed_tool_declines() {
        let previous = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut current = tools(&["c", "a"]);
        let before = current.clone();
        assert!(!stabilize_tool_order(&mut current, &previous));
        assert_eq!(current, before);
    }

    /// Moving a tool moves the provider's breakpoint with it. Defer to PR-E1 /
    /// PR-E3, which own ordering whenever a marker is present.
    #[test]
    fn cache_control_marker_declines() {
        let previous = vec!["a".to_string(), "b".to_string()];
        let mut current = vec![
            json!({"name": "b"}),
            json!({"name": "a", "cache_control": {"type": "ephemeral"}}),
        ];
        let before = current.clone();
        assert!(!stabilize_tool_order(&mut current, &previous));
        assert_eq!(current, before);
    }

    /// Reordering must move definitions, never rewrite them.
    #[test]
    fn definitions_are_preserved_byte_for_byte() {
        let previous = vec!["a".to_string(), "b".to_string()];
        let mut current = vec![
            json!({"name": "b", "description": "beta", "input_schema": {"z": 1, "a": 2}}),
            json!({"name": "a", "description": "alpha"}),
        ];
        let original = current.clone();
        assert!(stabilize_tool_order(&mut current, &previous));
        assert_eq!(current[0], original[1]);
        assert_eq!(current[1], original[0]);
    }

    /// Two tools sharing a name must both survive the reorder.
    #[test]
    fn duplicate_names_are_not_dropped() {
        let previous = vec!["a".to_string(), "a".to_string(), "b".to_string()];
        let mut current = vec![
            json!({"name": "b"}),
            json!({"name": "a", "tag": 1}),
            json!({"name": "a", "tag": 2}),
        ];
        assert!(stabilize_tool_order(&mut current, &previous));
        assert_eq!(names(&current), ["a", "a", "b"]);
        assert_eq!(current[0]["tag"], json!(1));
        assert_eq!(current[1]["tag"], json!(2));
    }

    /// OpenAI-shaped definitions are named one level down.
    #[test]
    fn openai_function_shape_is_matched_by_name() {
        let previous = vec!["a".to_string(), "b".to_string()];
        let mut current = vec![
            json!({"type": "function", "function": {"name": "b"}}),
            json!({"type": "function", "function": {"name": "a"}}),
        ];
        assert!(stabilize_tool_order(&mut current, &previous));
        assert_eq!(names(&current), ["a", "b"]);
    }

    /// Nothing to replay against on the first turn, but the order must be
    /// recorded so the second turn can act.
    #[test]
    fn store_records_on_first_sight_then_stabilizes() {
        let store = ToolOrderStore::new(4);
        let mut first = tools(&["a", "b", "c"]);
        assert!(!store.stabilize("sess", "opus", &mut first));

        let mut second = tools(&["a", "new", "b", "c"]);
        assert!(store.stabilize("sess", "opus", &mut second));
        assert_eq!(names(&second), ["a", "b", "c", "new"]);
    }

    /// A subagent's tool set is a strict subset of the main agent's, and the
    /// credential-derived session key is shared between them. Without the model
    /// in the key the subagent's order would be replayed onto the main agent's
    /// array.
    #[test]
    fn model_separates_agents_sharing_a_session_key() {
        let store = ToolOrderStore::new(4);
        let mut sub = tools(&["b", "a"]);
        store.stabilize("sess", "sonnet", &mut sub);

        let mut main = tools(&["a", "b", "c"]);
        assert!(!store.stabilize("sess", "opus", &mut main));
        assert_eq!(names(&main), ["a", "b", "c"]);
    }

    /// A declined turn must re-anchor: the provider cached what we forwarded,
    /// not the order we were remembering.
    #[test]
    fn declined_turn_reanchors_the_record() {
        let store = ToolOrderStore::new(4);
        store.stabilize("sess", "opus", &mut tools(&["a", "b", "c"]));

        // "b" disappears — declined, and the record becomes [c, a].
        let mut shrunk = tools(&["c", "a"]);
        assert!(!store.stabilize("sess", "opus", &mut shrunk));

        // Next turn extends [c, a]; replay must follow the new anchor.
        let mut grown = tools(&["a", "new", "c"]);
        assert!(store.stabilize("sess", "opus", &mut grown));
        assert_eq!(names(&grown), ["c", "a", "new"]);
    }
}
