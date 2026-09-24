//! Divergence diagnostics between two message lists.
//!
//! Moved out of `prefix_replay.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

/// First structural path at which two canonicalized values differ.
///
/// Keys and array indices only — **never a value**. The point is to name the
/// field that churns (`content[0].text`, `content[2].source.data`) so a false
/// divergence can be told from a real edit in a single sample instead of a
/// distribution, and tool-result content must not reach the log to do it.
///
/// Returns `None` when the values agree.
pub fn first_structural_difference(a: &Value, b: &Value) -> Option<String> {
    fn walk(a: &Value, b: &Value, path: &mut String) -> bool {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                // A key present on one side and not the other is itself the
                // difference, and its name is the useful part.
                for k in x.keys().chain(y.keys()) {
                    match (x.get(k), y.get(k)) {
                        (Some(av), Some(bv)) => {
                            let mark = path.len();
                            path.push('.');
                            path.push_str(k);
                            if walk(av, bv, path) {
                                return true;
                            }
                            path.truncate(mark);
                        }
                        _ => {
                            path.push('.');
                            path.push_str(k);
                            return true;
                        }
                    }
                }
                false
            }
            (Value::Array(x), Value::Array(y)) => {
                if x.len() != y.len() {
                    path.push_str(&format!("[len {} vs {}]", x.len(), y.len()));
                    return true;
                }
                for (i, (av, bv)) in x.iter().zip(y).enumerate() {
                    let mark = path.len();
                    path.push_str(&format!("[{i}]"));
                    if walk(av, bv, path) {
                        return true;
                    }
                    path.truncate(mark);
                }
                false
            }
            _ => a != b,
        }
    }
    let mut path = String::new();
    if walk(a, b, &mut path) {
        Some(path.trim_start_matches('.').to_string())
    } else {
        None
    }
}

/// Name the field that made two leading messages disagree.
///
/// Computed by the caller only when logging a
/// [`ReplaySkip::PrefixContentDiverged`], so a turn that replays cleanly never
/// pays for it. Structure only — see [`first_structural_difference`].
pub fn describe_divergence(
    previous_originals: &[Value],
    current_originals: &[Value],
    index: usize,
) -> Option<String> {
    let prev = previous_originals.get(index)?;
    let cur = current_originals.get(index)?;
    first_structural_difference(
        &canonicalize_for_prefix_compare(prev),
        &canonicalize_for_prefix_compare(cur),
    )
}

/// How much of the differing text reaches the log, in characters.
pub(super) const DIFF_TEXT_HEAD_CHARS: usize = 120;

/// The head of the text a divergence sits in, on each side.
///
/// `first_diff_path` says a mismatch is at `content[0].text` but not what it
/// is, and that gap cost a whole investigation: the cause was trailing
/// whitespace, which had to be inferred from message shapes when reading a
/// hundred characters of the two strings would have shown it.
///
/// This is the one place the rule against logging values is relaxed, so it is
/// held tight: the first [`DIFF_TEXT_HEAD_CHARS`] characters only, escaped so
/// no control byte or newline can break the line, and only for the message the
/// path already names. The text is the canonical form — scaffolding filtered,
/// edges trimmed — the same pair [`describe_divergence`] compares, so the path
/// and the text can never disagree. Returns `None` when the two agree or the
/// path points at something that is not text, in which case the shape fields
/// already say what changed.
pub fn divergence_text_heads(
    previous_originals: &[Value],
    current_originals: &[Value],
    index: usize,
) -> Option<(String, String)> {
    let prev = canonicalize_for_prefix_compare(previous_originals.get(index)?);
    let cur = canonicalize_for_prefix_compare(current_originals.get(index)?);
    let path = first_structural_difference(&prev, &cur)?;
    Some((text_head_at(&prev, &path), text_head_at(&cur, &path)))
}

/// The escaped head of the string at `path`, empty when it is not a string.
///
/// Reads the paths [`first_structural_difference`] writes — dotted keys with
/// `[i]` indices. `content[len 2 vs 1]` and friends do not resolve, and are
/// meant not to: a block came or went, so there is no differing text to show.
pub(super) fn text_head_at(value: &Value, path: &str) -> String {
    let mut cursor = value;
    for segment in path.split('.') {
        let (key, mut rest) = match segment.find('[') {
            Some(open) => segment.split_at(open),
            None => (segment, ""),
        };
        if !key.is_empty() {
            let Some(next) = cursor.get(key) else {
                return String::new();
            };
            cursor = next;
        }
        while let Some(close) = rest.find(']') {
            let Ok(index) = rest[1..close].parse::<usize>() else {
                return String::new();
            };
            let Some(next) = cursor.get(index) else {
                return String::new();
            };
            cursor = next;
            rest = &rest[close + 1..];
        }
    }
    cursor.as_str().map(escaped_head).unwrap_or_default()
}

/// First [`DIFF_TEXT_HEAD_CHARS`] characters, escaped for a log line.
///
/// `escape_debug` is what makes the whitespace case readable: a trailing
/// newline is the difference that started this, and it prints as `\n` rather
/// than as nothing at all.
pub(super) fn escaped_head(text: &str) -> String {
    let mut head: String = text
        .chars()
        .take(DIFF_TEXT_HEAD_CHARS)
        .flat_map(char::escape_debug)
        .collect();
    if text.chars().nth(DIFF_TEXT_HEAD_CHARS).is_some() {
        head.push('…');
    }
    head
}

/// What kind of `text` blocks a message carries, from a closed vocabulary.
///
/// `block_type_shape` says a `text` block appeared or vanished; it cannot say
/// what the block was, and that is the whole question when the same index
/// churns every turn. A client's ephemeral scaffolding and a real edit look
/// identical as `text`.
///
/// The vocabulary is fixed — `system-reminder`, `other-tag`, `plain` — so
/// nothing user-controlled reaches the log. Reporting the actual tag name would
/// break that, because a tag is as attacker- and user-controlled as the body.
pub fn text_block_kinds(message: &Value) -> String {
    let Some(content) = message.get("content") else {
        return String::new();
    };
    // String content is classified too. Returning empty for it — as this did
    // until 2026-08-13 — made `diff_text_kinds` read `'' -> ''` across a
    // divergence whose cause was a reminder living inside the string. That
    // looks exactly like "no reminder involved" and cost a wrong diagnosis.
    if let Some(text) = content.as_str() {
        return text_kind(text).to_string();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .map(|b| text_kind(b.get("text").and_then(|t| t.as_str()).unwrap_or("")))
        .collect::<Vec<_>>()
        .join(",")
}

/// Closed vocabulary — `system-reminder`, `plain+system-reminder`, `other-tag`,
/// `plain`. Nothing user-controlled reaches the log.
pub(super) fn text_kind(text: &str) -> &'static str {
    let trimmed = text.trim_start();
    if trimmed.starts_with(SYSTEM_REMINDER_OPEN_TAG) {
        "system-reminder"
    } else if text.contains(SYSTEM_REMINDER_OPEN_TAG) {
        // Reminder sitting after real text. Reported apart from `plain`
        // because this is the shape the filter used to miss entirely.
        "plain+system-reminder"
    } else if trimmed.starts_with('<') {
        "other-tag"
    } else {
        "plain"
    }
}

/// The sequence of content-block `type` values in a message, e.g.
/// `"tool_result,text"`.
///
/// Types only — never the blocks' contents. A divergence reported as
/// `content[len 2 vs 1]` says a block vanished but not which kind, and that is
/// the difference between a tool result being collapsed by something in front
/// of the client and an ordinary message being edited. Safe to log for the
/// same reason [`first_structural_difference`] is: the vocabulary is a fixed
/// set of API type names, not user data.
pub fn block_type_shape(message: &Value) -> String {
    match message.get("content") {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|b| b.get("type").and_then(Value::as_str).unwrap_or("?"))
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::String(_)) => "string".to_string(),
        _ => String::new(),
    }
}
