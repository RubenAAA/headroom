//! One definition of the ephemeral `<system-reminder>` scaffolding Claude Code
//! attaches to and withdraws from its own history.
//!
//! Split out of `prefix_replay` on 2026-08-26 because two subsystems were
//! answering the same question differently. `prefix_replay` has been blind to
//! this churn since the relocation pass was removed on 2026-08-16; the drift
//! detector was not, and it runs first and drops the whole stored prefix
//! (`proxy.rs`, the rebuild boundary). The same withdrawal was therefore free
//! at message 47 and cost a full re-cache at message 1 — 5 events and 430,296
//! tokens between 2026-08-23 and 08-26, worst single turn 145,891.
//!
//! Nothing here is new logic. It is the `prefix_replay` code, moved, so that a
//! future change cannot make the two callers disagree again.

use serde_json::Value;

/// A `<system-reminder>` the client attaches to the newest message and withdraws
/// on the following turn.
///
/// Same shape of problem as a proactive expansion, arrived at from the other
/// end. Measured 2026-08-09: the client hangs one of these off a `tool_result`
/// for exactly one turn, so the provider caches a prefix ending in a block that
/// will not be there next time. The prefix then breaks at that message and
/// everything after it is re-written — 95 turns, 4,353,443 tokens, an average of
/// 45,826 re-written for a few hundred bytes of reminder, 19% of the day's
/// input bill.
///
/// Nothing here removes or moves the block: the model still sees it, in place,
/// on the turn it arrives. It is only kept out of the *cached* region, which is
/// where its disappearance does the damage.
pub(crate) const SYSTEM_REMINDER_OPEN_TAG: &str = "<system-reminder>";

pub(crate) const SYSTEM_REMINDER_CLOSE_TAG: &str = "</system-reminder>";

pub(crate) fn is_ephemeral_client_text(text: &str) -> bool {
    text.trim_start().starts_with(SYSTEM_REMINDER_OPEN_TAG)
}

pub(crate) fn is_ephemeral_client_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(is_ephemeral_client_text)
}

/// Lift every `<system-reminder>…</system-reminder>` span out of `text`.
///
/// Returns the remaining text and the spans, in order. The client does not
/// always give a reminder its own block: it also arrives inline, in the middle
/// of a plain string message. [`is_ephemeral_client_text`] only sees the block
/// form, because it tests the *start* of the text, so an inline one survived
/// into the comparison key on the turn it arrived and vanished from it on the
/// turn the client re-shaped or withdrew it. The two keys then differed at that
/// message — always the newest one, so always the tail of the stored prefix —
/// and the whole prefix was re-written. Measured 2026-08-13 in one session:
/// four declines, 507,201 tokens, 67% of everything that session cached.
///
/// A span that never closes is left alone. The client always closes these, and
/// swallowing to end-of-text would eat real content on a malformed one.
pub(crate) fn split_ephemeral_spans(text: &str) -> (String, Vec<String>) {
    let mut kept = String::with_capacity(text.len());
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find(SYSTEM_REMINDER_OPEN_TAG) {
        let Some(close) = rest[open..].find(SYSTEM_REMINDER_CLOSE_TAG) else {
            break;
        };
        let end = open + close + SYSTEM_REMINDER_CLOSE_TAG.len();
        kept.push_str(&rest[..open]);
        spans.push(rest[open..end].to_string());
        rest = &rest[end..];
    }
    kept.push_str(rest);
    if spans.is_empty() {
        return (kept, spans);
    }
    // Lifting a span leaves the whitespace that separated it from the real
    // text. The client's own block-form version of the same message does not
    // carry that whitespace, so without this the two shapes still differ by a
    // newline — and differ in the forwarded bytes, not just the key.
    (kept.trim().to_string(), spans)
}

pub(crate) fn block_carries_ephemeral_span(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|t| t.contains(SYSTEM_REMINDER_OPEN_TAG))
}

/// Scaffolding at the END of `text`: the spans that trail it, and the prose
/// before them. `None` when a span sits in the middle of prose.
///
/// The permissive [`split_ephemeral_spans`] is right for the comparison key and
/// wrong for anything that rewrites bytes. A turn that merely QUOTES the tag —
/// writing test cases for this file will do it — had the span lifted out of the
/// middle of its prose, and a block that held nothing else was left as `""`.
/// The model then read its own words back as an empty block.
///
/// Only a span the client appended may be moved, and an appended one is always
/// at the end. Anything else is prose that happens to contain the characters.
pub(crate) fn split_trailing_ephemeral_spans(text: &str) -> Option<(String, Vec<String>)> {
    let first_open = text.find(SYSTEM_REMINDER_OPEN_TAG)?;
    let (prose, trailing) = text.split_at(first_open);
    let (leftover, spans) = split_ephemeral_spans(trailing);
    // Anything left after the first span means prose follows it, so the spans
    // are embedded rather than appended. Leave the whole block alone.
    if spans.is_empty() || !leftover.is_empty() {
        return None;
    }
    Some((prose.trim_end().to_string(), spans))
}

/// [`split_trailing_ephemeral_spans`] for one block: what remains of it (`None`
/// when it held nothing else) and the spans taken. The outer `None` means the
/// block must not be touched at all.
pub(crate) fn take_trailing_ephemeral_spans(block: &Value) -> Option<(Option<Value>, Vec<String>)> {
    if !block_carries_ephemeral_span(block) {
        return None;
    }
    let text = block.get("text").and_then(Value::as_str)?;
    let (prose, spans) = split_trailing_ephemeral_spans(text)?;
    if prose.is_empty() {
        return Some((None, spans));
    }
    let mut kept = block.clone();
    if let Some(obj) = kept.as_object_mut() {
        obj.insert("text".to_string(), Value::String(prose));
    }
    Some((Some(kept), spans))
}

/// Whether the message at `index` is client scaffolding rather than conversation.
///
/// Widens [`is_pure_client_scaffolding`] with the marker the data says is the
/// real one: `role`. The Messages API carries the system prompt in a top-level
/// field, so a `role: "system"` entry inside `messages` never comes from the
/// user or from the model — only the client puts one there.
///
/// The `<system-reminder>` wrapper turned out to be optional. Of the 3,050
/// `role: "system"` messages across the 114 prefixes stored on 2026-08-26, 593
/// carry it and **81% do not**; the bare ones are output-style banners, hook
/// context, skill and agent listings and `Note:` file notices. Claude Code sends
/// the *same* `PreToolUse:Bash` text both ways — 468 tagged, 656 bare — so the
/// tag cannot be the test. Keying on it missed four in five, and the miss was
/// expensive: a withdrawal deep in a conversation broke the index alignment and
/// stranded the entire stored prefix, 1,998,513 tokens over 33 turns, 95% of all
/// `prefix_content_diverged` waste on record.
///
/// (That store is live, so the counts drift between readings. The ratio does
/// not, and the ratio is the finding.)
///
/// `index == 0` is excluded. An OpenAI-Chat body puts its real system prompt
/// there, and that one is content: if it goes, the prompt genuinely changed and
/// the prefix genuinely has to be rebuilt. The proxy's own converters
/// (`handlers/gemini.rs`, `handlers/batch.rs`, `handlers/local_model.rs`) each
/// push their system message first, and no conversation among the 114 stored
/// prefixes opens with one, so the exception costs nothing here and protects
/// them.
pub(crate) fn is_client_scaffolding_message(index: usize, message: &Value) -> bool {
    if index > 0 && message.get("role").and_then(Value::as_str) == Some("system") {
        return true;
    }
    is_pure_client_scaffolding(message)
}

/// Whether a message is nothing but client scaffolding — no prose of its own.
///
/// Claude Code does not only hang a `<system-reminder>` off a message it was
/// already sending. It also sends the reminder AS a message, most often with
/// `role: "system"` and a bare string body, and withdraws it a few turns later.
/// Every one of the 46 stored prefixes on disk on 2026-08-26 held at least one.
///
/// The distinction that matters is prose: a message the client would still be
/// sending if the reminder were gone has to be compared, because a real edit to
/// it is a real divergence. One that would not exist at all is scaffolding, and
/// its arrival or departure says nothing about the conversation.
///
/// `assistant` is excluded for the reason relocation learned the hard way: the
/// model quotes these tags when it discusses them, and its own words are not
/// scaffolding.
pub(crate) fn is_pure_client_scaffolding(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) == Some("assistant") {
        return false;
    }
    match message.get("content") {
        Some(Value::String(text)) => {
            split_trailing_ephemeral_spans(text).is_some_and(|(prose, _)| prose.is_empty())
        }
        // Every block nothing but spans. `is_ephemeral_client_block` is the
        // wrong test here and the difference is not academic: it asks only
        // whether the text STARTS with a reminder, so a lone block holding a
        // reminder and then the user's actual prompt would pass it and the
        // whole turn would be read as scaffolding.
        Some(Value::Array(blocks)) => {
            !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|block| matches!(take_trailing_ephemeral_spans(block), Some((None, _))))
        }
        _ => false,
    }
}
