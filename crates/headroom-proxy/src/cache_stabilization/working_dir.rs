//! Hold the working-directory line in the `system` preamble still.
//!
//! Claude Code's environment preamble states where the session is:
//!
//! ```text
//!  - Primary working directory: /home/user/headroom
//! ```
//!
//! and restates it further down. The line sits about a third of the way into a
//! 14,000-character block that carries no `cache_control` marker of its own, so
//! it is inside every cached prefix. Anthropic matches a prefix from byte 0.
//! Change the path and the conversation re-creates from the system block down.
//!
//! Measured against the `turn_cost_ledger` over 1,035 joined turns of the
//! 2026-08-17 capture (`bench/_syscost.py`, `bench/_sysedits.py`): one such edit
//! — a `cd` out of `.../apps/mobile` — cost **65,051 tokens of creation** on a
//! turn where its depth peers averaged 4,637. A single path shortening is worth
//! fourteen ordinary turns.
//!
//! So the values are pinned to the ones this conversation opened with. What the
//! model is told stays true a different way: when a pinned value displaces a
//! live one, the live one is stated at the message tail, which is outside the
//! cached prefix and costs nothing to change. Without that the model would be
//! told a stale directory and would build wrong absolute paths — the whole point
//! of this work is to spend less while behaving at least as well.
//!
//! The note survives into later turns for free. The client re-sends history
//! without it, but [`super::prefix_replay`] replays what we FORWARDED, and the
//! note is in the forwarded copy. If the overlay ever declines, the note is
//! dropped from history and re-stated on the new tail, so the model is never
//! left with only the stale preamble. That dependence is why the caller gates
//! this on `--prefix-replay`: with replay off the note would break the prefix at
//! the tail every turn, causing the churn it exists to stop.
//!
//! It is plain text, not a `<system-reminder>`. A reminder would read better,
//! but [`super::prefix_replay`] classifies a block opening with that tag as
//! client-ephemeral: the tail breakpoint is placed strictly before it and the
//! comparison key drops it. The note would then sit outside the cache and go
//! missing from the very prefix comparison it has to survive.
//!
//! # What this deliberately does not touch
//!
//! `system[0]`'s `cc_version` also churns, and in the same capture it cost
//! 180,148 tokens across two conversations. That was a client auto-update
//! (2.1.232 -> 2.1.233) landing while [`super::billing_header`]'s per-process
//! pin held the old string, followed by a proxy RESTART that re-latched the pin
//! to the new one. The churn is an artifact of restarting the proxy, not of
//! normal use, and the fix for it is to restart less rather than to persist a
//! version string the provider is entitled to see change.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;
use serde_json::Value;

/// Matches both `Primary working directory: ` and `Working directory: `, which
/// is the whole set of forms the preamble uses. Deliberately a literal scan
/// rather than a pattern: the value that follows is a path, and a pattern loose
/// enough to cover every path shape is loose enough to match prose.
const MARKER: &str = "orking directory: ";

/// How long a pin outlives its conversation's last turn.
///
/// Longer than the replay store's 600s session TTL on purpose. The provider
/// holds a 1h cached prefix, so a conversation can idle past the replay TTL and
/// still have a live prefix upstream; re-latching to the live directory at that
/// point would break the very entry this exists to protect.
const PIN_TTL: Duration = Duration::from_secs(2 * 3600);

/// Per-conversation working-directory pins.
#[derive(Clone)]
pub struct WorkingDirPins {
    pins: Arc<Mutex<LruCache<String, (Vec<String>, Instant)>>>,
}

impl std::fmt::Debug for WorkingDirPins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkingDirPins").finish_non_exhaustive()
    }
}

impl WorkingDirPins {
    /// Build a store bounded to `capacity` conversations.
    ///
    /// # Panics
    /// If `capacity == 0`.
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).expect("WorkingDirPins capacity must be > 0");
        Self {
            pins: Arc::new(Mutex::new(LruCache::new(cap))),
        }
    }

    /// Hold this conversation's working directories, and state the live one at
    /// the message tail when a pin displaces it.
    ///
    /// Returns the live directory it had to replace, or `None` when it changed
    /// nothing — first sight of the conversation, no such line in `system`, the
    /// live values already matching the pin, or a preamble whose shape changed.
    pub fn hold(&self, body: &mut Value, conversation_key: &str) -> Option<String> {
        let live = read_dirs(body.get("system")?);
        if live.is_empty() {
            return None;
        }

        let held = {
            let mut pins = self.pins.lock().expect("WorkingDirPins mutex poisoned");
            match pins.get(conversation_key) {
                // Expired: this conversation's prefix is long gone upstream, so
                // re-latch rather than resurrect a directory nobody is in.
                Some((_, latched)) if latched.elapsed() > PIN_TTL => {
                    pins.put(conversation_key.to_string(), (live.clone(), Instant::now()));
                    return None;
                }
                Some((held, _)) => held.clone(),
                None => {
                    pins.put(conversation_key.to_string(), (live.clone(), Instant::now()));
                    return None;
                }
            }
        };

        if held == live {
            return None;
        }
        // A different count means the preamble's shape changed, not the
        // directory. Substituting by position would put a path on the wrong
        // line, which is worse than a re-cache.
        if held.len() != live.len() {
            return None;
        }

        let system = body.get_mut("system")?;
        if !write_dirs(system, &held) {
            return None;
        }
        let live_dir = live.into_iter().next()?;
        note_at_tail(body, &live_dir, &held[0]);
        Some(live_dir)
    }
}

/// Every working directory named in `system`, in the order they appear.
fn read_dirs(system: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for text in system_texts(system) {
        let mut rest = text;
        while let Some(at) = rest.find(MARKER) {
            rest = &rest[at + MARKER.len()..];
            let end = rest
                .find(|c: char| c.is_whitespace())
                .unwrap_or(rest.len());
            if end > 0 {
                out.push(rest[..end].to_string());
            }
            rest = &rest[end..];
        }
    }
    out
}

/// Rewrite the working directories in `system` to `held`, positionally.
///
/// Returns whether every value was replaced. A `false` return leaves the blocks
/// partially rewritten, so the caller must only reach it with a count it has
/// already checked — which [`WorkingDirPins::hold`] does.
fn write_dirs(system: &mut Value, held: &[String]) -> bool {
    let mut next = held.iter();
    for text in system_texts_mut(system) {
        let mut out = String::with_capacity(text.len());
        let mut rest = text.as_str();
        while let Some(at) = rest.find(MARKER) {
            let after = at + MARKER.len();
            out.push_str(&rest[..after]);
            rest = &rest[after..];
            let end = rest
                .find(|c: char| c.is_whitespace())
                .unwrap_or(rest.len());
            match next.next() {
                Some(value) if end > 0 => out.push_str(value),
                _ => out.push_str(&rest[..end]),
            }
            rest = &rest[end..];
        }
        out.push_str(rest);
        *text = out;
    }
    next.next().is_none()
}

/// State the live directory on the newest message.
///
/// Only onto a `user` message with block content. A tool-result turn can end
/// with an assistant message, and appending a text block there would change
/// what the assistant is recorded as having said. Skipping costs the note on
/// that turn; the next user turn states it.
fn note_at_tail(body: &mut Value, live: &str, held: &str) {
    let text = format!(
        "Working directory is now {live}. The environment section above was \
         captured when this conversation started and still names {held}; \
         {live} is the current one."
    );
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(last) = messages.last_mut() else {
        return;
    };
    if last.get("role").and_then(Value::as_str) != Some("user") {
        return;
    }
    let Some(blocks) = last.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    // Idempotent: a retried turn must forward the same bytes, and a second note
    // would be a second entry for a prefix the provider already holds.
    if blocks.iter().any(|block| {
        block.get("text").and_then(Value::as_str) == Some(text.as_str())
    }) {
        return;
    }
    blocks.push(serde_json::json!({"type": "text", "text": text}));
}

/// The text of each `system` block, whichever shape `system` takes.
fn system_texts(system: &Value) -> Vec<&str> {
    match system {
        Value::String(text) => vec![text.as_str()],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// [`system_texts`] for rewriting.
fn system_texts_mut(system: &mut Value) -> Vec<&mut String> {
    match system {
        Value::String(text) => vec![text],
        Value::Array(blocks) => blocks
            .iter_mut()
            .filter_map(|block| match block.get_mut("text") {
                Some(Value::String(text)) => Some(text),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(dir: &str, tail_role: &str) -> Value {
        json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.233.8f7;"},
                {"type": "text", "text": format!(
                    "Here is useful information about the environment you are running in:\n\
                     <env>\n - Primary working directory: {dir}\n - Platform: linux\n</env>\n\
                     Working directory: {dir}\n")},
            ],
            "messages": [
                {"role": tail_role, "content": [{"type": "text", "text": "go"}]}
            ]
        })
    }

    fn dirs(body: &Value) -> Vec<String> {
        read_dirs(body.get("system").unwrap())
    }

    #[test]
    fn first_sight_latches_and_forwards_unchanged() {
        let pins = WorkingDirPins::new(4);
        let mut b = body("/repo", "user");
        let before = b.clone();
        assert_eq!(pins.hold(&mut b, "conv"), None);
        assert_eq!(b, before, "first turn must be byte-identical");
    }

    #[test]
    fn a_changed_directory_is_held_and_stated_at_the_tail() {
        let pins = WorkingDirPins::new(4);
        pins.hold(&mut body("/repo", "user"), "conv");

        let mut b = body("/repo/apps/mobile", "user");
        assert_eq!(pins.hold(&mut b, "conv").as_deref(), Some("/repo/apps/mobile"));
        assert_eq!(dirs(&b), vec!["/repo", "/repo"], "both lines held");

        let tail = b["messages"][0]["content"].as_array().unwrap();
        assert_eq!(tail.len(), 2, "the live directory is stated once");
        let note = tail[1]["text"].as_str().unwrap();
        assert!(note.contains("/repo/apps/mobile"), "states the live directory");
        assert!(note.contains("/repo"), "names the one the preamble still shows");
    }

    #[test]
    fn an_unchanged_directory_is_a_no_op() {
        let pins = WorkingDirPins::new(4);
        pins.hold(&mut body("/repo", "user"), "conv");
        let mut b = body("/repo", "user");
        let before = b.clone();
        assert_eq!(pins.hold(&mut b, "conv"), None);
        assert_eq!(b, before);
    }

    #[test]
    fn holding_is_idempotent_under_retry() {
        let pins = WorkingDirPins::new(4);
        pins.hold(&mut body("/repo", "user"), "conv");
        let mut first = body("/repo/apps/mobile", "user");
        pins.hold(&mut first, "conv");
        // The retry arrives as the client sent it, so it starts from the live
        // directory again and must come out byte-identical to the first attempt.
        let mut retry = body("/repo/apps/mobile", "user");
        pins.hold(&mut retry, "conv");
        assert_eq!(first, retry);
    }

    #[test]
    fn a_second_note_is_never_appended() {
        let mut b = body("/repo", "user");
        note_at_tail(&mut b, "/live", "/repo");
        note_at_tail(&mut b, "/live", "/repo");
        assert_eq!(b["messages"][0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn each_conversation_keeps_its_own_directory() {
        let pins = WorkingDirPins::new(4);
        pins.hold(&mut body("/a", "user"), "one");
        pins.hold(&mut body("/b", "user"), "two");

        let mut b = body("/elsewhere", "user");
        pins.hold(&mut b, "two");
        assert_eq!(dirs(&b), vec!["/b", "/b"]);
    }

    #[test]
    fn a_preamble_that_changed_shape_is_left_alone() {
        let pins = WorkingDirPins::new(4);
        pins.hold(&mut body("/repo", "user"), "conv");

        // One line instead of two: the block was rewritten, not the directory.
        let mut b = json!({
            "system": [{"type": "text", "text": " - Primary working directory: /other\n"}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "go"}]}]
        });
        let before = b.clone();
        assert_eq!(pins.hold(&mut b, "conv"), None);
        assert_eq!(b, before);
    }

    #[test]
    fn a_body_with_no_working_directory_is_untouched() {
        let pins = WorkingDirPins::new(4);
        let mut b = json!({
            "system": [{"type": "text", "text": "You are Claude Code."}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "go"}]}]
        });
        let before = b.clone();
        assert_eq!(pins.hold(&mut b, "conv"), None);
        assert_eq!(b, before);
    }

    #[test]
    fn an_assistant_tail_is_held_without_a_note() {
        let pins = WorkingDirPins::new(4);
        pins.hold(&mut body("/repo", "user"), "conv");
        let mut b = body("/elsewhere", "assistant");
        assert_eq!(pins.hold(&mut b, "conv").as_deref(), Some("/elsewhere"));
        assert_eq!(dirs(&b), vec!["/repo", "/repo"]);
        assert_eq!(
            b["messages"][0]["content"].as_array().unwrap().len(),
            1,
            "nothing is appended to what the assistant said"
        );
    }

    #[test]
    fn a_string_system_prompt_is_held_too() {
        let pins = WorkingDirPins::new(4);
        let mut first = json!({
            "system": " - Primary working directory: /repo\n",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "go"}]}]
        });
        pins.hold(&mut first, "conv");
        let mut b = json!({
            "system": " - Primary working directory: /moved\n",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "go"}]}]
        });
        assert_eq!(pins.hold(&mut b, "conv").as_deref(), Some("/moved"));
        assert_eq!(b["system"].as_str().unwrap(), " - Primary working directory: /repo\n");
    }

    #[test]
    fn a_trailing_directory_with_no_newline_is_read() {
        let system = json!([{"type": "text", "text": "Working directory: /repo"}]);
        assert_eq!(read_dirs(&system), vec!["/repo"]);
    }
}
