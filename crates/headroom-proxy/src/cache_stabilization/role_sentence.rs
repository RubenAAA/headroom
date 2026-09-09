//! Hold the opening role sentence of the `system` preamble still.
//!
//! Claude Code opens its main system block with one sentence naming what the
//! agent is. Two forms are in circulation and the client swaps between them
//! mid-session:
//!
//! ```text
//! You are an interactive agent that helps users with software engineering tasks.
//! You are an interactive agent that helps users according to your "Output Style", which describes how you should respond to user queries.
//! ```
//!
//! The swap tracks whether the client believes an output style is active. It
//! is not driven by anything the operator changes: no settings file moved, and
//! four unrelated sessions flipped within 70 seconds of each other on
//! 2026-09-07, then flipped back nine minutes later. The sentence sits at the
//! top of a 14,000-character block with no `cache_control` marker of its own,
//! so every flip re-creates the conversation from the system block down. Eight
//! flips that day cost 788,210 cached tokens, the third-largest source of waste
//! behind two defects since fixed.
//!
//! The two forms are one instruction said two ways. The `# Output Style`
//! section further down the block is what carries the style itself, and it is
//! unchanged across a flip. So the hold is a plain substitution: the sentence a
//! conversation opened with is the sentence it keeps, and nothing is added at
//! the tail because there is nothing the model needs told.
//!
//! Sibling of [`super::working_dir`], which holds the directory line in the
//! same block for the same reason and explains the pin lifetime and the
//! preview-then-restore dance the drift hash needs.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;
use serde_json::Value;

use super::Hold;

/// The sentence opens with this and runs to the first period. Anchored on the
/// literal head so that no other prose in the block can match.
const HEAD: &str = "You are an interactive agent that helps users ";

/// How long a pin outlives its conversation's last turn. Matches
/// [`super::working_dir`]: longer than the replay store's session TTL because
/// the provider holds a 1h prefix.
const PIN_TTL: Duration = Duration::from_secs(2 * 3600);

/// Per-conversation opening-sentence pins.
#[derive(Clone)]
pub struct RoleSentencePins {
    pins: Arc<Mutex<LruCache<String, (String, Instant)>>>,
}

impl std::fmt::Debug for RoleSentencePins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoleSentencePins").finish_non_exhaustive()
    }
}

impl RoleSentencePins {
    /// Build a store bounded to `capacity` conversations.
    ///
    /// # Panics
    /// If `capacity == 0`.
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).expect("RoleSentencePins capacity must be > 0");
        Self {
            pins: Arc::new(Mutex::new(LruCache::new(cap))),
        }
    }

    /// Hold this conversation's opening sentence.
    ///
    /// Answers what it did: [`Hold::Held`] carries the live sentence it
    /// replaced, and every other variant names why it changed nothing —
    /// first sight of the conversation, no such sentence in `system`, or
    /// the live sentence already matching the pin.
    pub fn hold(&self, body: &mut Value, conversation_key: &str) -> Hold {
        let Some(system) = body.get("system") else {
            return Hold::Absent;
        };
        let Some(live) = read_sentence(system) else {
            return Hold::Absent;
        };

        let held = {
            let mut pins = self.pins.lock().expect("RoleSentencePins mutex poisoned");
            match pins.get(conversation_key) {
                Some((_, latched)) if latched.elapsed() > PIN_TTL => {
                    pins.put(conversation_key.to_string(), (live, Instant::now()));
                    return Hold::Relatched;
                }
                Some((held, _)) => held.clone(),
                None => {
                    pins.put(conversation_key.to_string(), (live, Instant::now()));
                    return Hold::Latched;
                }
            }
        };

        if held == live {
            return Hold::Matched;
        }
        let Some(system) = body.get_mut("system") else {
            return Hold::NotWritable;
        };
        if !write_sentence(system, &held) {
            return Hold::NotWritable;
        }
        Hold::Held(live)
    }

    /// Rewrite the sentence to the held value and hand back the client's
    /// `system`, without latching a pin. The drift hash must see the body as it
    /// will be forwarded; see [`super::working_dir::WorkingDirPins::preview`].
    pub fn preview(&self, body: &mut Value, conversation_key: &str) -> Option<Value> {
        let live = read_sentence(body.get("system")?)?;

        let held = {
            let pins = self.pins.lock().expect("RoleSentencePins mutex poisoned");
            match pins.peek(conversation_key) {
                Some((held, latched)) if latched.elapsed() <= PIN_TTL => held.clone(),
                _ => return None,
            }
        };

        if held == live {
            return None;
        }

        let system = body.get("system")?.clone();
        if !write_sentence(body.get_mut("system")?, &held) {
            *body.get_mut("system")? = system;
            return None;
        }
        Some(system)
    }

    /// Copy another lane's pin entry onto this lane, when this lane has none.
    /// Same contract as [`super::working_dir::WorkingDirPins::inherit_pin`]:
    /// the donor is a message-lineage match from the replay store, never an
    /// unrelated stream, and the latch instant travels with the entry.
    pub fn inherit_pin(&self, from_key: &str, to_key: &str) -> bool {
        if from_key == to_key {
            return false;
        }
        let mut pins = self.pins.lock().expect("RoleSentencePins mutex poisoned");
        if pins.peek(to_key).is_some() {
            return false;
        }
        let Some(entry) = pins.peek(from_key).cloned() else {
            return false;
        };
        pins.put(to_key.to_string(), entry);
        true
    }
}

/// The opening sentence, from [`HEAD`] through its closing period, taken from
/// the first `system` text that carries one.
fn read_sentence(system: &Value) -> Option<String> {
    system_texts(system).into_iter().find_map(|text| {
        let at = text.find(HEAD)?;
        let rest = &text[at..];
        let end = rest.find('.')?;
        Some(rest[..=end].to_string())
    })
}

/// Replace the opening sentence with `held` in the first `system` text that
/// carries one. Returns whether a replacement happened.
fn write_sentence(system: &mut Value, held: &str) -> bool {
    for text in system_texts_mut(system) {
        let Some(at) = text.find(HEAD) else {
            continue;
        };
        let Some(len) = text[at..].find('.') else {
            continue;
        };
        text.replace_range(at..=at + len, held);
        return true;
    }
    false
}

/// The `text` of every block in `system`, or the bare string itself.
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

    const PLAIN: &str =
        "You are an interactive agent that helps users with software engineering tasks.";
    const STYLED: &str = "You are an interactive agent that helps users according to your \
         \"Output Style\", which describes how you should respond to user queries.";

    fn body(sentence: &str) -> Value {
        json!({
            "system": [
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
                {"type": "text", "text": format!("{sentence} Use the instructions below.\n\n# Output Style\nConcise.")},
            ],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        })
    }

    fn sentence_of(body: &Value) -> String {
        read_sentence(body.get("system").unwrap()).unwrap()
    }

    #[test]
    fn first_sight_latches_and_changes_nothing() {
        let pins = RoleSentencePins::new(4);
        let mut b = body(PLAIN);
        let before = b.clone();
        assert_eq!(pins.hold(&mut b, "c1").rewrote(), None);
        assert_eq!(b, before, "first sight is a byte-equal passthrough");
    }

    /// Same contract as the working-dir pins: a fresh lane inherits its
    /// lineage donor's sentence, and never overwrites its own.
    #[test]
    fn inherit_pin_lends_and_never_overwrites() {
        let pins = RoleSentencePins::new(4);
        pins.hold(&mut body(PLAIN), "lane-a");

        assert!(pins.inherit_pin("lane-a", "lane-b"));
        let mut b = body(STYLED);
        assert_eq!(pins.hold(&mut b, "lane-b").rewrote(), Some(STYLED));
        assert_eq!(sentence_of(&b), PLAIN);

        assert!(
            !pins.inherit_pin("lane-a", "lane-b"),
            "lane-b owns its pin now"
        );
        assert!(!pins.inherit_pin("ghost", "lane-c"));
    }

    #[test]
    fn a_flipped_sentence_is_held_to_the_opening_form() {
        let pins = RoleSentencePins::new(4);
        pins.hold(&mut body(PLAIN), "c1");

        let mut b = body(STYLED);
        assert_eq!(pins.hold(&mut b, "c1").rewrote(), Some(STYLED));
        assert_eq!(sentence_of(&b), PLAIN);
        let text = b["system"][1]["text"].as_str().unwrap();
        assert!(
            text.ends_with("# Output Style\nConcise."),
            "only the sentence changes; the rest of the block is untouched"
        );
        assert_eq!(
            b["system"][0]["text"], "You are Claude Code, Anthropic's official CLI for Claude.",
            "blocks without the sentence are untouched"
        );
    }

    #[test]
    fn the_hold_is_symmetric() {
        let pins = RoleSentencePins::new(4);
        pins.hold(&mut body(STYLED), "c1");

        let mut b = body(PLAIN);
        assert_eq!(pins.hold(&mut b, "c1").rewrote(), Some(PLAIN));
        assert_eq!(sentence_of(&b), STYLED);
    }

    #[test]
    fn a_matching_sentence_is_a_passthrough() {
        let pins = RoleSentencePins::new(4);
        pins.hold(&mut body(PLAIN), "c1");
        let mut b = body(PLAIN);
        let before = b.clone();
        assert_eq!(pins.hold(&mut b, "c1").rewrote(), None);
        assert_eq!(b, before);
    }

    /// The outcomes a caller logs. Each no-op has its own name because
    /// "the hold never ran" and "the hold ran and had nothing to do" are
    /// the same absence of a `role_sentence_held` line otherwise.
    #[test]
    fn every_outcome_is_named() {
        let pins = RoleSentencePins::new(4);

        let mut absent = json!({"system": "Be helpful.", "messages": []});
        assert_eq!(pins.hold(&mut absent, "c1"), Hold::Absent);

        let mut first = body(PLAIN);
        assert_eq!(pins.hold(&mut first, "c2"), Hold::Latched);

        let mut again = body(PLAIN);
        assert_eq!(pins.hold(&mut again, "c2"), Hold::Matched);

        let mut flipped = body(STYLED);
        assert_eq!(
            pins.hold(&mut flipped, "c2"),
            Hold::Held(STYLED.to_string())
        );
        assert_eq!(
            sentence_of(&flipped),
            PLAIN,
            "flip was held to the opening form"
        );
    }

    #[test]
    fn labels_are_stable_for_logs() {
        assert_eq!(Hold::Held(String::new()).label(), "held");
        assert_eq!(Hold::Latched.label(), "latched");
        assert_eq!(Hold::Matched.label(), "matched");
        assert_eq!(Hold::Absent.label(), "absent");
        assert_eq!(Hold::Reshaped.label(), "reshaped");
        assert_eq!(Hold::Relatched.label(), "relatched");
        assert_eq!(Hold::NotWritable.label(), "not_writable");
    }

    #[test]
    fn only_the_outcomes_worth_reading_are_noteworthy() {
        // The healthy steady state happens on every turn and must not
        // reach the proxy's default `info` level.
        assert!(!Hold::Held(String::new()).is_noteworthy());
        assert!(!Hold::Matched.is_noteworthy());
        assert!(!Hold::Latched.is_noteworthy());
        // Each of these means the hold wanted to act and could not.
        assert!(Hold::Absent.is_noteworthy());
        assert!(Hold::Reshaped.is_noteworthy());
        assert!(Hold::Relatched.is_noteworthy());
        assert!(Hold::NotWritable.is_noteworthy());
    }

    #[test]
    fn pins_are_per_conversation() {
        let pins = RoleSentencePins::new(4);
        pins.hold(&mut body(PLAIN), "c1");
        let mut other = body(STYLED);
        assert_eq!(
            pins.hold(&mut other, "c2"),
            Hold::Latched,
            "c2's first sight"
        );
        assert_eq!(sentence_of(&other), STYLED);
    }

    #[test]
    fn no_sentence_means_no_pin_and_no_change() {
        let pins = RoleSentencePins::new(4);
        let mut b = json!({"system": "Be helpful.", "messages": []});
        assert_eq!(pins.hold(&mut b, "c1").rewrote(), None);
        assert_eq!(b["system"], "Be helpful.");
        let mut later = body(PLAIN);
        assert_eq!(
            pins.hold(&mut later, "c1"),
            Hold::Latched,
            "still first sight"
        );
    }

    #[test]
    fn string_system_is_held_too() {
        let pins = RoleSentencePins::new(4);
        let mut first = json!({"system": format!("{PLAIN} More."), "messages": []});
        pins.hold(&mut first, "c1");
        let mut b = json!({"system": format!("{STYLED} More."), "messages": []});
        assert_eq!(pins.hold(&mut b, "c1").rewrote(), Some(STYLED));
        assert_eq!(b["system"], format!("{PLAIN} More."));
    }

    #[test]
    fn preview_rewrites_without_latching_and_returns_the_original() {
        let pins = RoleSentencePins::new(4);
        assert_eq!(
            pins.preview(&mut body(STYLED), "c1"),
            None,
            "nothing pinned yet"
        );
        pins.hold(&mut body(PLAIN), "c1");

        let mut b = body(STYLED);
        let original = b["system"].clone();
        let returned = pins.preview(&mut b, "c1").expect("pinned, so previewed");
        assert_eq!(returned, original, "hands back the client's own system");
        assert_eq!(sentence_of(&b), PLAIN, "the body now shows the held form");

        let mut again = body(STYLED);
        assert_eq!(pins.preview(&mut again, "c1"), Some(original));
    }

    #[test]
    fn preview_is_a_passthrough_when_the_sentence_already_matches() {
        let pins = RoleSentencePins::new(4);
        pins.hold(&mut body(PLAIN), "c1");
        let mut b = body(PLAIN);
        assert_eq!(pins.preview(&mut b, "c1"), None);
    }

    #[test]
    fn hold_is_idempotent_across_a_retry() {
        let pins = RoleSentencePins::new(4);
        pins.hold(&mut body(PLAIN), "c1");
        let mut first = body(STYLED);
        pins.hold(&mut first, "c1");
        let mut second = body(STYLED);
        pins.hold(&mut second, "c1");
        assert_eq!(first, second, "a retried turn forwards the same bytes");
    }
}
