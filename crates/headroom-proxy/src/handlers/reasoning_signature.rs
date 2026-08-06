//! Stateless replay envelope for Codex encrypted reasoning.
//!
//! Reasoning models resume their chain of thought only if the previous turn's
//! `reasoning` items are handed back verbatim. Those items carry an opaque
//! `encrypted_content` blob that the backend binds to the model that emitted
//! it, and Anthropic's message shape has nowhere to put them — so a translating
//! proxy has to carry them itself.
//!
//! The obvious way is a proxy-side cache keyed by session. That is what we did,
//! and it goes wrong in every direction: the key has to include the model or a
//! `/model` switch replays one model's blobs into another's request, the cache
//! dies with the process, and it needs eviction caps that silently stop caching
//! in long sessions.
//!
//! So don't hold the state. Pack `id` + `encrypted_content` into the `signature`
//! of the `thinking` block we hand the client, and unpack it when the client
//! echoes that block back. Claude Code round-trips content blocks already, so
//! the conversation carries its own reasoning and there is nothing to key,
//! evict, or lose on restart.
//!
//! Envelope format and bounds follow raine/claude-code-proxy (MIT),
//! `src/providers/codex/translate/reasoning_signature.rs`. The prefix is ours,
//! so envelopes from another proxy decode to `None` and are dropped rather than
//! replayed as garbage.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::Value;

const PREFIX: &str = "headroom:codex:v1:";
const MAX_ID_BYTES: usize = 4 * 1024;
const MAX_ENCRYPTED_CONTENT_BYTES: usize = 8 * 1024 * 1024;

/// A reasoning item reduced to the two fields that must survive a round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningReplay {
    pub id: String,
    pub encrypted_content: String,
}

/// Accumulates a reasoning item's identity across the stream events that
/// describe it.
///
/// `id` and `encrypted_content` do not have to arrive together:
/// `output_item.added` may announce the id and `output_item.done` carry the
/// blob, or `.done` may repeat only the id. Capturing both and keeping the
/// first non-empty value of each means neither event alone has to be complete.
#[derive(Debug, Clone, Default)]
pub struct PendingReasoning {
    id: Option<String>,
    encrypted_content: Option<String>,
}

impl PendingReasoning {
    pub fn capture(&mut self, item: &Value) {
        if let Some(id) = non_empty_string(item.get("id")) {
            self.id = Some(id.to_string());
        }
        if let Some(encrypted_content) = non_empty_string(item.get("encrypted_content")) {
            self.encrypted_content = Some(encrypted_content.to_string());
        }
    }

    /// The captured pair, or `None` if either half is still missing.
    pub fn replay(&self) -> Option<ReasoningReplay> {
        Some(ReasoningReplay {
            id: self.id.clone()?,
            encrypted_content: self.encrypted_content.clone()?,
        })
    }

    pub fn reset(&mut self) {
        self.id = None;
        self.encrypted_content = None;
    }
}

/// Pack a reasoning item into a `thinking` block signature.
///
/// The id is base64'd because it shares the field with the separator; the blob
/// is already opaque base64-ish text and is appended as-is, which keeps the
/// common case free of a second encode pass over something that can reach
/// megabytes.
pub fn encode_reasoning_signature(replay: &ReasoningReplay) -> Option<String> {
    if replay.id.is_empty()
        || replay.id.len() > MAX_ID_BYTES
        || replay.encrypted_content.is_empty()
        || replay.encrypted_content.len() > MAX_ENCRYPTED_CONTENT_BYTES
    {
        return None;
    }
    let encoded_id = URL_SAFE_NO_PAD.encode(replay.id.as_bytes());
    Some(format!("{PREFIX}{encoded_id}:{}", replay.encrypted_content))
}

/// Unpack a signature written by [`encode_reasoning_signature`].
///
/// Returns `None` for anything else — a real Anthropic thinking signature, an
/// envelope from a different proxy, or a malformed one. Callers drop the block
/// in that case rather than guessing.
pub fn decode_reasoning_signature(signature: &str) -> Option<ReasoningReplay> {
    let payload = signature.strip_prefix(PREFIX)?;
    if payload.is_empty() || payload.len() > max_payload_len() {
        return None;
    }
    let (encoded_id, encrypted_content) = payload.split_once(':')?;
    if encoded_id.is_empty()
        || encoded_id.len() > encoded_id_len_limit()
        || encrypted_content.is_empty()
        || encrypted_content.len() > MAX_ENCRYPTED_CONTENT_BYTES
    {
        return None;
    }
    let id = URL_SAFE_NO_PAD.decode(encoded_id).ok()?;
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return None;
    }
    Some(ReasoningReplay {
        id: String::from_utf8(id).ok()?,
        encrypted_content: encrypted_content.to_string(),
    })
}

/// The Responses `input` item a decoded envelope turns back into.
pub fn reasoning_input_item(replay: ReasoningReplay) -> Value {
    serde_json::json!({
        "type": "reasoning",
        "id": replay.id,
        "summary": [],
        "encrypted_content": replay.encrypted_content,
    })
}

fn encoded_id_len_limit() -> usize {
    MAX_ID_BYTES.div_ceil(3) * 4
}

fn max_payload_len() -> usize {
    encoded_id_len_limit() + 1 + MAX_ENCRYPTED_CONTENT_BYTES
}

fn non_empty_string(value: Option<&Value>) -> Option<&str> {
    value?.as_str().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn signature_round_trip_preserves_reasoning_identity() {
        let replay = ReasoningReplay {
            id: "rs_1".to_string(),
            encrypted_content: "gAAAAopaque".to_string(),
        };
        let signature = encode_reasoning_signature(&replay).unwrap();
        assert!(signature.starts_with(PREFIX));
        assert!(signature.ends_with(":gAAAAopaque"));
        assert_eq!(decode_reasoning_signature(&signature), Some(replay));
    }

    #[test]
    fn foreign_and_malformed_signatures_are_ignored() {
        // A real Anthropic thinking signature must not be mistaken for ours.
        assert_eq!(decode_reasoning_signature("ErUBCkYIBRgCKkDzS1nT"), None);
        // Another proxy's envelope.
        assert_eq!(decode_reasoning_signature("ccp:codex:v1:cnNfMQ:blob"), None);
        assert_eq!(decode_reasoning_signature("headroom:codex:v1:"), None);
        assert_eq!(decode_reasoning_signature("headroom:codex:v1:no-sep"), None);
        assert_eq!(
            decode_reasoning_signature("headroom:codex:v1:not-base64!!:blob"),
            None
        );
    }

    #[test]
    fn incomplete_pairs_never_encode() {
        assert_eq!(
            encode_reasoning_signature(&ReasoningReplay {
                id: String::new(),
                encrypted_content: "blob".to_string(),
            }),
            None
        );
        assert_eq!(
            encode_reasoning_signature(&ReasoningReplay {
                id: "rs_1".to_string(),
                encrypted_content: String::new(),
            }),
            None
        );
    }

    #[test]
    fn pending_reasoning_keeps_early_metadata_when_done_omits_it() {
        let mut pending = PendingReasoning::default();
        pending.capture(&json!({"id": "rs_1", "encrypted_content": "early"}));
        pending.capture(&json!({"id": "rs_1"}));
        assert_eq!(
            pending.replay(),
            Some(ReasoningReplay {
                id: "rs_1".to_string(),
                encrypted_content: "early".to_string(),
            })
        );
    }

    #[test]
    fn pending_reasoning_needs_both_halves() {
        let mut pending = PendingReasoning::default();
        pending.capture(&json!({"id": "rs_1"}));
        assert_eq!(pending.replay(), None);
        pending.capture(&json!({"encrypted_content": "blob"}));
        assert!(pending.replay().is_some());
        pending.reset();
        assert_eq!(pending.replay(), None);
    }

    #[test]
    fn oversized_signature_is_ignored_without_decoding() {
        let signature = format!(
            "{PREFIX}cnNfMQ:{}",
            "A".repeat(MAX_ENCRYPTED_CONTENT_BYTES + 1)
        );
        assert_eq!(decode_reasoning_signature(&signature), None);
    }

    #[test]
    fn decoded_envelope_rebuilds_the_responses_item() {
        let item = reasoning_input_item(ReasoningReplay {
            id: "rs_9".to_string(),
            encrypted_content: "blob".to_string(),
        });
        assert_eq!(item["type"], "reasoning");
        assert_eq!(item["id"], "rs_9");
        assert_eq!(item["encrypted_content"], "blob");
        assert_eq!(item["summary"], json!([]));
    }
}
