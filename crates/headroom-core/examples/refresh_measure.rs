//! Scratch measurement for the Jev context-refresh idea: how many bytes of a
//! captured Claude Code transcript do the existing deterministic passes remove?
//! Usage: refresh_measure <transcript.jsonl>
use headroom_core::transforms::TextCrusher;
use headroom_core::transforms::cross_turn_dedup::{
    DedupBlock, dedup_blocks, dedup_messages, dedup_messages_with_user_text,
};
use serde_json::Value;

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("transcript path");
    let raw = std::fs::read_to_string(path).unwrap();
    let mut messages: Vec<Value> = Vec::new();
    for line in raw.lines() {
        let Ok(o) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(m) = o.get("message") else { continue };
        if m.get("model").and_then(Value::as_str) == Some("<synthetic>") {
            continue;
        }
        messages.push(m.clone());
    }
    let size = |ms: &[Value]| serde_json::to_string(ms).unwrap().len();
    let tr_bytes = |ms: &[Value]| -> usize {
        ms.iter()
            .filter_map(|m| m.get("content")?.as_array())
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            .map(|b| text_of(&b["content"]).len())
            .sum()
    };
    println!(
        "messages {} total json {} tool_result text {}",
        messages.len(),
        size(&messages),
        tr_bytes(&messages)
    );

    // 1. cross-turn dedup over tool results, as the proxy runs it.
    let mut m1 = messages.clone();
    let s = dedup_messages(&mut m1, 0);
    println!(
        "dedup tool_results: spans {} lines {} chars_removed {} -> tool_result text {} (total json {})",
        s.spans_folded,
        s.lines_removed,
        s.chars_removed,
        tr_bytes(&m1),
        size(&m1)
    );

    // 1b. the shipped user-text variant, newest 8 messages kept verbatim.
    let user_text_bytes = |ms: &[Value]| -> usize {
        ms.iter()
            .filter(|m| m["role"] == "user")
            .map(|m| match &m["content"] {
                Value::String(s) => s.len(),
                Value::Array(a) => a
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .map(|b| b["text"].as_str().map_or(0, str::len))
                    .sum(),
                _ => 0,
            })
            .sum()
    };
    let mut m2 = messages.clone();
    let s2 = dedup_messages_with_user_text(&mut m2, 0, 8);
    println!(
        "dedup_messages_with_user_text(tail 8): spans {} chars_removed {} user text {} -> {} (total json {} -> {})",
        s2.spans_folded,
        s2.chars_removed,
        user_text_bytes(&messages),
        user_text_bytes(&m2),
        size(&messages),
        size(&m2)
    );

    // 2. same algorithm on user-role text blocks (not wired for them today).
    let mut texts = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if m["role"] != "user" {
            continue;
        }
        match &m["content"] {
            Value::String(s) => texts.push(DedupBlock::new(s.clone(), i)),
            Value::Array(a) => {
                for b in a {
                    if b["type"] == "text" {
                        texts.push(DedupBlock::new(
                            b["text"].as_str().unwrap_or("").to_string(),
                            i,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    let before: usize = texts.iter().map(|b| b.text.len()).sum();
    let (out, s) = dedup_blocks(&texts);
    let after: usize = out.iter().map(|b| b.text.len()).sum();
    println!(
        "dedup user text: blocks {} spans {} chars_removed {} bytes {} -> {}",
        s.blocks, s.spans_folded, s.chars_removed, before, after
    );

    // 3. text_crusher (lossy, extractive, ratio 0.5) per category, on all but the newest 20 messages.
    let crusher = TextCrusher::default();
    let ctx = "current task";
    let keep = messages.len().saturating_sub(20);
    let mut cat: std::collections::BTreeMap<&str, (usize, usize, usize)> = Default::default();
    let mut crush = |name: &'static str, t: &str| {
        let e = cat.entry(name).or_default();
        e.0 += t.len();
        if t.len() < 600 {
            e.1 += t.len();
            e.2 += t.len();
            return;
        }
        let r = crusher.compress(t, ctx, None);
        e.1 += r.compressed.len();
        e.2 += r.compressed.len().min(t.len());
    };
    for (i, m) in messages.iter().enumerate() {
        if i >= keep {
            break;
        }
        let role = m["role"].as_str().unwrap_or("");
        let blocks: Vec<Value> = match &m["content"] {
            Value::String(s) => vec![serde_json::json!({"type":"text","text":s})],
            Value::Array(a) => a.clone(),
            _ => vec![],
        };
        for b in blocks {
            match (role, b["type"].as_str().unwrap_or("")) {
                ("user", "text") => crush("user text", b["text"].as_str().unwrap_or("")),
                ("assistant", "text") => crush("assistant text", b["text"].as_str().unwrap_or("")),
                ("user", "tool_result") => crush("tool_result", &text_of(&b["content"])),
                ("assistant", "tool_use") if b["name"] == "Agent" => {
                    crush("Agent prompt", b["input"]["prompt"].as_str().unwrap_or(""))
                }
                _ => {}
            }
        }
    }
    for (k, (before, after, _)) in cat {
        println!(
            "text_crusher {k}: {before} -> {after} ({:.0}%)",
            100.0 * after as f64 / before.max(1) as f64
        );
    }

    stage1(&messages);
}

/// Content bytes by kind, plus encrypted-reasoning envelope characters.
fn census(ms: &[Value]) -> (usize, usize, usize) {
    let (mut text, mut tools, mut blob) = (0, 0, 0);
    for m in ms {
        match &m["content"] {
            Value::String(s) => text += s.len(),
            Value::Array(a) => {
                for b in a {
                    match b["type"].as_str().unwrap_or("") {
                        "text" => text += b["text"].as_str().map_or(0, str::len),
                        "tool_use" => tools += b["input"].to_string().len(),
                        "tool_result" => tools += text_of(&b["content"]).len(),
                        "thinking" => {
                            text += b["thinking"].as_str().map_or(0, str::len);
                            blob += b["signature"].as_str().map_or(0, str::len);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    (text, tools, blob)
}

/// The agreed stage 1, in order, on everything but the newest 20 messages:
/// drop old reasoning blobs, fold repeats (user text included), crush the rest.
fn stage1(original: &[Value]) {
    const TAIL: usize = 20;
    const BLOB_CHARS_PER_TOKEN: f64 = 8.5; // measured 7.5 to 10
    let mut ms = original.to_vec();
    let report = |label: &str, ms: &[Value]| {
        let (t, u, b) = census(ms);
        let tokens = (t + u) as f64 / 4.0 + b as f64 / BLOB_CHARS_PER_TOKEN;
        println!(
            "stage1 {label:<22} text {t:>8} tools {u:>8} blob chars {b:>8} ~{:>4.0}k tokens",
            tokens / 1000.0
        );
    };
    report("start", &ms);
    let keep = ms.len().saturating_sub(TAIL);
    for m in ms.iter_mut().take(keep) {
        if let Some(a) = m["content"].as_array_mut() {
            a.retain(|b| b["type"] != "thinking");
        }
    }
    report("drop old thinking", &ms);
    dedup_messages_with_user_text(&mut ms, 0, 8);
    report("dedup incl. user text", &ms);
    if let Ok(path) = std::env::var("STAGE1_DUMP") {
        std::fs::write(path, serde_json::to_string(&ms).unwrap()).unwrap();
    }

    let task = original
        .iter()
        .rev()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| match &m["content"] {
            Value::String(s) => Some(s.clone()),
            Value::Array(a) => a
                .iter()
                .find_map(|b| b["text"].as_str().map(str::to_string)),
            _ => None,
        })
        .find(|t| t.len() >= 20 && !t.starts_with('<'))
        .unwrap_or_default();
    let crusher = TextCrusher::default();
    let mut shown = false;
    let mut crush = |t: &str| -> Option<String> {
        if t.len() < 600 {
            return None;
        }
        let r = crusher.compress(t, &task, None);
        if r.compressed.len() >= t.len() {
            return None;
        }
        if !shown && t.starts_with("<task-notification>") {
            shown = true;
            println!(
                "--- sample before ({} chars):\n{}\n--- after ({} chars):\n{}\n---",
                t.len(),
                &t[..t.len().min(700)],
                r.compressed.len(),
                &r.compressed[..r.compressed.len().min(700)]
            );
        }
        Some(r.compressed)
    };
    for m in ms.iter_mut().take(keep) {
        let role = m["role"].as_str().unwrap_or("").to_string();
        match &mut m["content"] {
            Value::String(s) if role != "assistant" || s.len() >= 600 => {
                if let Some(c) = crush(s) {
                    *s = c;
                }
            }
            Value::Array(a) => {
                for b in a {
                    match b["type"].as_str().unwrap_or("") {
                        "text" => {
                            if let Some(c) = crush(b["text"].as_str().unwrap_or("")) {
                                b["text"] = Value::String(c);
                            }
                        }
                        "tool_result" => {
                            if let Value::String(s) = &b["content"]
                                && let Some(c) = crush(s)
                            {
                                b["content"] = Value::String(c);
                            }
                        }
                        "tool_use" if b["name"] == "Agent" => {
                            if let Some(c) = crush(b["input"]["prompt"].as_str().unwrap_or("")) {
                                b["input"]["prompt"] = Value::String(c);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    report("crush old prose", &ms);
}
