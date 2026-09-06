//! B3 — pin the tool roster a session offers, so one tool flapping in and
//! out of the client's `tools[]` does not throw away the cached prefix.
//!
//! The provider's cache prefix is `tools → system → messages`. Anthropic
//! hashes the tools array as sent, so a roster that loses one tool between
//! turns is a new prefix from the first byte: every tool after it, the whole
//! system prompt, and the whole history recache. Claude Code does this
//! intermittently (`SendUserFile` drops out of the roster on a turn and comes
//! back the next; `WaitForMcpServers` toggles), and the live log attributes
//! about half of all recache waste to it (`cache_recache_observed` with
//! `attribution_reason = "tools"`, joined to `tool_roster_changed`).
//!
//! This module remembers, per `(session_key, model)`, every tool definition
//! the session has offered, in first-seen order. On a later turn:
//!
//! - a remembered tool the client did **not** send is re-inserted at its
//!   remembered position, with the definition we last saw for it;
//! - a tool the client sent is passed through untouched and its definition
//!   becomes the remembered one, so a schema change while the tool is present
//!   is the client's own change and recaches on its own merits;
//! - a genuinely new tool goes to the **end**, so the prefix before it still
//!   hits.
//!
//! The pin never forgets a name for the life of the entry. That is the
//! point — a "removed" tool is, on the evidence, a flap — and it is also the
//! risk: if the client really dropped the tool, the model may still call it.
//! The proxy has no view of what the client does with such a call, so the
//! feature is a flag ([`crate::config::Config::cache_pin_tool_roster`]).
//!
//! Declines, byte for byte, whenever any tool carries a `cache_control`
//! marker — same rule as B2 ([`super::tool_order`]) — so it stays out of the
//! way of a caller managing their own tool breakpoints. Runs before B2 so B2
//! sees a complete roster and its subset guard passes.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use lru::LruCache;
use serde_json::Value;

/// Production session capacity — matches B2's.
pub const ROSTER_PIN_STORE_CAPACITY: usize = 1000;

/// The name a tool definition is addressed by, in either the Anthropic shape
/// (`{"name": ...}`) or the OpenAI function shape (`{"function": {"name": ...}}`).
fn tool_name(tool: &Value) -> Option<&str> {
    tool.get("name")
        .and_then(Value::as_str)
        .or_else(|| tool.get("function")?.get("name")?.as_str())
}

fn any_tool_marked(tools: &[Value]) -> bool {
    tools.iter().any(|t| t.get("cache_control").is_some())
}

/// What one turn's pin did to the roster. Both empty means the bytes went
/// out as the client sent them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PinOutcome {
    /// Remembered tools the client omitted this turn, put back in place.
    pub reinserted: Vec<String>,
    /// Tools seen for the first time, moved to the tail of the array.
    pub appended: Vec<String>,
}

impl PinOutcome {
    pub fn changed(&self) -> bool {
        !self.reinserted.is_empty() || !self.appended.is_empty()
    }
}

/// Rebuild `tools` against `remembered` in place, and update `remembered` to
/// what went out.
///
/// The result leads with every remembered name in remembered order — the
/// client's definition where it sent one, the remembered definition where it
/// did not — followed by the client's new tools in their original relative
/// order. Duplicate names in the client's array keep every copy: the first
/// one takes the remembered slot, the rest stay where the client put them,
/// after the remembered block, so the pin cannot drop a definition.
///
/// Declines (returns an empty outcome, `tools` untouched) when any tool
/// carries a `cache_control` marker. It still records the roster in that
/// case, so a later turn without markers has something to pin to.
pub fn pin_tool_roster(tools: &mut Vec<Value>, remembered: &mut Roster) -> PinOutcome {
    if any_tool_marked(tools) {
        remember(tools, remembered);
        return PinOutcome::default();
    }

    let mut current: Vec<Option<Value>> = tools.drain(..).map(Some).collect();
    let mut outcome = PinOutcome::default();
    let mut out = Vec::with_capacity(current.len() + remembered.len());

    for (name, def) in remembered.iter_mut() {
        let hit = current
            .iter()
            .position(|t| t.as_ref().and_then(tool_name) == Some(name.as_str()));
        match hit {
            Some(i) => {
                let t = current[i].take().expect("slot claimed once");
                *def = t.clone();
                out.push(t);
            }
            None => {
                outcome.reinserted.push(name.clone());
                out.push(def.clone());
            }
        }
    }

    for t in current.into_iter().flatten() {
        if let Some(name) = tool_name(&t) {
            if !remembered.iter().any(|(n, _)| n == name) {
                remembered.push((name.to_string(), t.clone()));
                outcome.appended.push(name.to_string());
            }
        }
        out.push(t);
    }

    // First sighting of a session: nothing was remembered, so nothing moved.
    // Report it as a no-op so the caller forwards the client's bytes as-is.
    if outcome.reinserted.is_empty() && out.len() == outcome.appended.len() {
        outcome.appended.clear();
    }

    *tools = out;
    outcome
}

/// Record `tools` into `remembered` without rewriting anything: refresh the
/// definition of every remembered name that is present, append the rest.
fn remember(tools: &[Value], remembered: &mut Roster) {
    for t in tools {
        let Some(name) = tool_name(t) else { continue };
        match remembered.iter_mut().find(|(n, _)| n == name) {
            Some((_, def)) => *def = t.clone(),
            None => remembered.push((name.to_string(), t.clone())),
        }
    }
}

/// Every tool one session has offered, in first-seen order, with the
/// definition last seen for each.
type Roster = Vec<(String, Value)>;

/// Per-session memory of every tool a session has offered, keyed on
/// `(session_key, model)` like [`super::tool_order::ToolOrderStore`].
#[derive(Clone)]
pub struct RosterPinStore {
    inner: Arc<Mutex<LruCache<String, Roster>>>,
}

impl std::fmt::Debug for RosterPinStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RosterPinStore").finish_non_exhaustive()
    }
}

impl Default for RosterPinStore {
    fn default() -> Self {
        Self::new(ROSTER_PIN_STORE_CAPACITY)
    }
}

impl RosterPinStore {
    pub fn new(capacity: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(capacity))),
        }
    }

    /// Pin `tools` to what this session has offered before, and remember the
    /// result. Holds the lock for the rewrite so two turns of one session
    /// in flight at once cannot interleave their memory.
    pub fn pin(&self, session_key: &str, model: &str, tools: &mut Vec<Value>) -> PinOutcome {
        let key = format!("{session_key}\u{1f}{model}");
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let remembered = guard.get_or_insert_mut(key, Vec::new);
        pin_tool_roster(tools, remembered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(name: &str) -> Value {
        json!({"name": name, "input_schema": {"type": "object"}})
    }

    fn names(tools: &[Value]) -> Vec<&str> {
        tools.iter().filter_map(tool_name).collect()
    }

    #[test]
    fn first_turn_records_and_moves_nothing() {
        let mut remembered = Vec::new();
        let mut tools = vec![t("a"), t("b"), t("c")];
        let before = tools.clone();
        let out = pin_tool_roster(&mut tools, &mut remembered);
        assert_eq!(out, PinOutcome::default());
        assert_eq!(tools, before);
        assert_eq!(remembered.len(), 3);
    }

    #[test]
    fn a_dropped_tool_comes_back_at_its_old_position() {
        let mut remembered = Vec::new();
        let mut first = vec![t("a"), t("send_user_file"), t("c")];
        pin_tool_roster(&mut first, &mut remembered);

        let mut second = vec![t("a"), t("c")];
        let out = pin_tool_roster(&mut second, &mut remembered);
        assert_eq!(out.reinserted, ["send_user_file"]);
        assert!(out.appended.is_empty());
        assert_eq!(names(&second), ["a", "send_user_file", "c"]);
        assert_eq!(
            second, first,
            "the roster must be byte-identical to turn one"
        );
    }

    #[test]
    fn a_new_tool_goes_to_the_tail() {
        let mut remembered = Vec::new();
        let mut first = vec![t("a"), t("b")];
        pin_tool_roster(&mut first, &mut remembered);

        let mut second = vec![t("a"), t("wait_for_mcp"), t("b")];
        let out = pin_tool_roster(&mut second, &mut remembered);
        assert_eq!(out.appended, ["wait_for_mcp"]);
        assert_eq!(names(&second), ["a", "b", "wait_for_mcp"]);

        // ...and stays there once it flaps out again.
        let mut third = vec![t("a"), t("b")];
        let out = pin_tool_roster(&mut third, &mut remembered);
        assert_eq!(out.reinserted, ["wait_for_mcp"]);
        assert_eq!(names(&third), ["a", "b", "wait_for_mcp"]);
    }

    #[test]
    fn a_present_tool_refreshes_its_remembered_definition() {
        let mut remembered = Vec::new();
        let mut first = vec![t("a"), t("b")];
        pin_tool_roster(&mut first, &mut remembered);

        let changed =
            json!({"name": "b", "input_schema": {"type": "object", "properties": {"x": {}}}});
        let mut second = vec![t("a"), changed.clone()];
        let out = pin_tool_roster(&mut second, &mut remembered);
        assert!(!out.changed());
        assert_eq!(
            second[1], changed,
            "the client's own definition passes through"
        );

        let mut third = vec![t("a")];
        pin_tool_roster(&mut third, &mut remembered);
        assert_eq!(
            third[1], changed,
            "reinsertion uses the last definition seen"
        );
    }

    #[test]
    fn a_marked_tool_declines_but_still_records() {
        let mut remembered = Vec::new();
        let mut first = vec![t("a"), t("b")];
        pin_tool_roster(&mut first, &mut remembered);

        let mut marked = vec![json!({"name": "a", "cache_control": {"type": "ephemeral"}})];
        let before = marked.clone();
        let out = pin_tool_roster(&mut marked, &mut remembered);
        assert!(!out.changed());
        assert_eq!(marked, before);
        assert_eq!(
            remembered.len(),
            2,
            "b is still remembered through the decline"
        );
        assert_eq!(remembered[0].1, before[0], "a's definition was refreshed");
    }

    #[test]
    fn duplicate_names_keep_every_copy() {
        let mut remembered = Vec::new();
        let mut first = vec![t("a"), t("b")];
        pin_tool_roster(&mut first, &mut remembered);

        let mut second = vec![t("b"), t("b")];
        let out = pin_tool_roster(&mut second, &mut remembered);
        assert_eq!(out.reinserted, ["a"]);
        assert_eq!(names(&second), ["a", "b", "b"]);
    }

    #[test]
    fn openai_function_shape_is_addressed_by_its_inner_name() {
        let f = |n: &str| json!({"type": "function", "function": {"name": n}});
        let mut remembered = Vec::new();
        let mut first = vec![f("a"), f("b")];
        pin_tool_roster(&mut first, &mut remembered);
        let mut second = vec![f("b")];
        let out = pin_tool_roster(&mut second, &mut remembered);
        assert_eq!(out.reinserted, ["a"]);
        assert_eq!(names(&second), ["a", "b"]);
    }

    #[test]
    fn store_keys_on_session_and_model() {
        let store = RosterPinStore::default();
        let mut a = vec![t("a"), t("b")];
        store.pin("sess", "opus", &mut a);

        let mut other_model = vec![t("b")];
        let out = store.pin("sess", "sonnet", &mut other_model);
        assert!(
            !out.changed(),
            "a different model must not inherit the roster"
        );

        let mut same = vec![t("b")];
        let out = store.pin("sess", "opus", &mut same);
        assert_eq!(out.reinserted, ["a"]);
    }
}
