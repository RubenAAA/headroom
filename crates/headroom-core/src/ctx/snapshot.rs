//! CTX-4 — resume snapshot + recall rendering (pure, deterministic).
//!
//! A Rust port of the *spirit* of context-mode's `session/snapshot.ts`
//! `buildResumeSnapshot`: a reference-based XML table-of-contents over stored
//! session events, where each section embeds a literal runnable `ctx_search`
//! call rather than dumping all data inline.
//!
//! # Determinism (invariants I1/I4, `docs/ctx-mode-in-headroom-plan.md`)
//!
//! Both renderers are **pure functions of their inputs** and emit **nothing
//! volatile**. The TS wrapper carries a `generated_at="${now}"` attribute; we
//! deliberately **drop it** — an injected block is decided once and replayed
//! byte-for-byte every turn, so a timestamp would bust the cache. Event
//! ordering is by the caller-supplied slice order (the store returns events
//! `ORDER BY id ASC`, matching TS `ORDER BY created_at ASC`).

use super::sessions::StoredEvent;
use super::store::SearchHit;

/// Max events rendered per section (parity with TS `MAX_ACTIVE_FILES` scale).
const MAX_PER_SECTION: usize = 10;
/// Max `ctx_search` query terms embedded per section (TS `maxQueries`).
const MAX_QUERIES: usize = 4;
/// Max chars of a rendered recent-message line (TS `RECENT_MESSAGE_MAX_CHARS`).
const RECENT_MESSAGE_MAX_CHARS: usize = 400;
/// Recent user messages rendered (TS `RECENT_MESSAGES_LIMIT`).
const RECENT_MESSAGES_LIMIT: usize = 3;

/// Sentinel prefix that marks an injected block, so the request path can detect
/// a body it already injected into and never double-inject (idempotency).
pub const INJECT_SENTINEL: &str = "<!--ctx:injected-->";

/// Build a resume snapshot from a prior conversation's stored events. Pure and
/// deterministic; no timestamps. `compact_count` renders into the wrapper
/// (defaults to 1 in the TS).
pub fn build_resume_snapshot(events: &[StoredEvent], compact_count: u64) -> String {
    let mut sections: Vec<String> = Vec::new();

    sections.push(how_to_search());

    // Category → section title, in TS push order (subset we actually capture).
    let plan: &[(&str, &str)] = &[
        ("intent", "session_goal"),
        ("file", "files"),
        ("error", "errors"),
        ("decision", "decisions"),
        ("rule", "rules"),
        ("git", "git"),
    ];
    for (category, title) in plan {
        if let Some(section) = render_section(events, category, title) {
            sections.push(section);
        }
    }

    if let Some(recent) = render_recent_user_messages(events) {
        sections.push(recent);
    }

    let body = sections.join("\n\n");
    format!(
        "{INJECT_SENTINEL}\n<session_resume events=\"{}\" compact_count=\"{}\">\n\n{body}\n\n</session_resume>",
        events.len(),
        compact_count,
    )
}

/// Build a fresh-conversation recall block from content-store search hits and a
/// short static directive. Pure and deterministic.
pub fn build_recall(hits: &[SearchHit], queries: &[String]) -> String {
    let mut lines = String::new();
    for hit in hits.iter().take(MAX_PER_SECTION) {
        lines.push_str(&format!(
            "- [{}] {}\n",
            hit.source,
            one_line(&hit.content, 200)
        ));
    }
    let body = if lines.is_empty() {
        "(no prior context found)".to_string()
    } else {
        lines.trim_end().to_string()
    };

    format!(
        "{INJECT_SENTINEL}\n<session_recall>\n{}\n<relevant_context>\n{body}\n</relevant_context>\n{}\n</session_recall>",
        search_toc(queries),
        session_directive(),
    )
}

/// Static "how to search" TOC header used by the resume snapshot.
fn how_to_search() -> String {
    "<how_to_search>\nSearch prior session context with the Bash tool:\n  headroom ctx search \"<query>\"\n</how_to_search>".to_string()
}

/// A minimal static session directive (spirit of `buildSessionDirective`).
/// Fixed text — no volatile fields.
fn session_directive() -> String {
    "<directive>\nPrior context is summarized above. Retrieve full detail on demand with `headroom ctx search`; do not assume anything not shown here.\n</directive>".to_string()
}

/// Render one category's section: the events' data as bullet lines plus a
/// literal `ctx_search` TOC. `None` when the category has no events.
fn render_section(events: &[StoredEvent], category: &str, title: &str) -> Option<String> {
    let items: Vec<&StoredEvent> = events.iter().filter(|e| e.category == category).collect();
    if items.is_empty() {
        return None;
    }

    // Dedup data lines, preserve order, cap.
    let mut seen = std::collections::HashSet::new();
    let mut lines = String::new();
    for e in items.iter().take(MAX_PER_SECTION) {
        let line = one_line(&e.data, 200);
        if seen.insert(line.clone()) {
            lines.push_str(&format!("- {line}\n"));
        }
    }

    // Queries derived from the item data (dedup, cap, per-term length cap).
    let queries: Vec<String> = {
        let mut seen_q = std::collections::HashSet::new();
        let mut out = Vec::new();
        for e in &items {
            let q = one_line(&e.data, 80);
            if !q.is_empty() && seen_q.insert(q.clone()) {
                out.push(q);
                if out.len() >= MAX_QUERIES {
                    break;
                }
            }
        }
        out
    };

    Some(format!(
        "<{title}>\n{}{}\n</{title}>",
        lines,
        search_toc(&queries)
    ))
}

/// Render the last few user-prompt/intent messages as a trailing section.
fn render_recent_user_messages(events: &[StoredEvent]) -> Option<String> {
    let mut msgs: Vec<&StoredEvent> = events
        .iter()
        .filter(|e| e.category == "intent" || e.category == "user-prompt")
        .collect();
    if msgs.is_empty() {
        return None;
    }
    // Last N in chronological order.
    let start = msgs.len().saturating_sub(RECENT_MESSAGES_LIMIT);
    msgs = msgs.split_off(start);

    let mut lines = String::new();
    for e in &msgs {
        lines.push_str(&format!(
            "- {}\n",
            one_line(&e.data, RECENT_MESSAGE_MAX_CHARS)
        ));
    }
    Some(format!(
        "<recent_user_messages>\n{}</recent_user_messages>",
        lines
    ))
}

/// A literal, runnable `ctx_search` TOC line for a section's queries.
fn search_toc(queries: &[String]) -> String {
    if queries.is_empty() {
        return String::new();
    }
    let quoted: Vec<String> = queries
        .iter()
        .take(MAX_QUERIES)
        .map(|q| format!("\"{}\"", q.replace('"', "'")))
        .collect();
    format!(
        "  For full details: headroom ctx search {}",
        quoted.join(" ")
    )
}

/// Collapse whitespace/newlines to a single line and cap to `max` chars
/// (codepoint-safe), matching the TS single-line rendering.
fn one_line(s: &str, max: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        collapsed.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(category: &str, data: &str) -> StoredEvent {
        StoredEvent {
            id: 0,
            session_id: "s".to_string(),
            type_: category.to_string(),
            category: category.to_string(),
            priority: 2,
            data: data.to_string(),
            project_dir: String::new(),
            attribution_source: "unknown".to_string(),
            attribution_confidence: 0.0,
            bytes_avoided: 0,
            bytes_returned: 0,
            source_hook: "ctx-observer".to_string(),
            created_at: String::new(),
            data_hash: String::new(),
        }
    }

    #[test]
    fn snapshot_has_wrapper_and_no_volatile_fields() {
        let events = vec![
            ev("intent", "add recall injection"),
            ev("file", "src/lib.rs"),
            ev("error", "exit code 1: build failed"),
            ev("git", "git commit -m wip"),
        ];
        let snap = build_resume_snapshot(&events, 2);
        // Structural invariants (golden parity deferred to CTX-4b).
        assert!(snap.starts_with(INJECT_SENTINEL));
        assert!(snap.contains("<session_resume events=\"4\" compact_count=\"2\">"));
        assert!(snap.contains("</session_resume>"));
        assert!(snap.contains("<session_goal>"));
        assert!(snap.contains("<files>"));
        assert!(snap.contains("<errors>"));
        assert!(snap.contains("<git>"));
        assert!(snap.contains("headroom ctx search"));
        // No volatile fields (I1): no generated_at, no ISO timestamps.
        assert!(!snap.contains("generated_at"));
        assert!(!snap.contains("T00:00:00"));
    }

    #[test]
    fn snapshot_is_deterministic() {
        let events = vec![ev("intent", "do the thing"), ev("file", "a.rs")];
        assert_eq!(
            build_resume_snapshot(&events, 1),
            build_resume_snapshot(&events, 1)
        );
    }

    #[test]
    fn empty_sections_are_omitted() {
        let events = vec![ev("intent", "only an intent")];
        let snap = build_resume_snapshot(&events, 1);
        assert!(snap.contains("<session_goal>"));
        assert!(!snap.contains("<files>"));
        assert!(!snap.contains("<errors>"));
    }

    #[test]
    fn recall_renders_hits_and_directive() {
        let hits = vec![SearchHit {
            title: "cat big.log".to_string(),
            content: "ERROR: disk full while writing".to_string(),
            source: "cat big.log".to_string(),
            rank: -1.0,
            content_type: "prose".to_string(),
            highlighted: String::new(),
            timestamp: None,
            session_id: "s".to_string(),
            match_layer: "rrf",
        }];
        let recall = build_recall(&hits, &["disk full".to_string()]);
        assert!(recall.starts_with(INJECT_SENTINEL));
        assert!(recall.contains("<session_recall>"));
        assert!(recall.contains("disk full"));
        assert!(recall.contains("<directive>"));
        assert!(!recall.contains("generated_at"));
    }

    #[test]
    fn recall_handles_no_hits() {
        let recall = build_recall(&[], &["q".to_string()]);
        assert!(recall.contains("(no prior context found)"));
    }
}
