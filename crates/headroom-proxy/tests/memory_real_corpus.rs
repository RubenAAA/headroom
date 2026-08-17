//! Can BM25 find the right memory among the real ones?
//!
//! The store round trip works on fixtures. This runs the actual markdown files
//! that Claude Code's auto-memory holds — frontmatter, wiki links, tables and
//! all — and asks whether searching for what a memory is *about* returns that
//! memory. That is the question the migration turns on, and fixtures cannot
//! answer it.
//!
//! Skipped when the directory is absent, so it does not break a clean checkout.

use std::sync::Arc;

use headroom_proxy::memory::ctx_backend::CtxMemoryBackend;
use headroom_proxy::memory::handler::{MemoryConfig, MemoryHandler, MemoryMode};
use headroom_proxy::memory::tool_adapter::Provider;
use serde_json::json;

const MEMORY_DIR: &str = "/home/user/.claude-work/projects/-home-user-headroom/memory";

fn handler(dir: &std::path::Path, top_k: usize) -> MemoryHandler {
    let mut handler = MemoryHandler::new(
        MemoryConfig {
            enabled: true,
            backend_name: "local".to_string(),
            inject_tools: true,
            mode: MemoryMode::Tool,
            top_k,
            ..Default::default()
        },
        "test",
    );
    handler.set_backend(Arc::new(CtxMemoryBackend::open(dir).unwrap()));
    handler
}

async fn call(handler: &MemoryHandler, tool: &str, input: serde_json::Value) -> String {
    let response = json!({
        "content": [{"type": "tool_use", "id": "t1", "name": tool, "input": input}]
    });
    let results = handler
        .handle_memory_tool_calls(&response, "default", Provider::Anthropic, None)
        .await;
    serde_json::to_string(&results).unwrap()
}

/// Load every real memory file. Returns the names that went in.
async fn load_real_memories(handler: &MemoryHandler) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir(MEMORY_DIR) else {
        return names;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        if name == "MEMORY" {
            continue; // the index, not a memory
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        call(
            handler,
            "memory_save",
            json!({"content": content, "title": name.replace('-', " ")}),
        )
        .await;
        names.push(name);
    }
    names
}

#[tokio::test]
async fn every_real_memory_file_loads_and_stays() {
    let dir = tempfile::tempdir().unwrap();
    let handler = handler(dir.path(), 200);

    let names = load_real_memories(&handler).await;
    if names.is_empty() {
        eprintln!("no memory dir on this machine; skipping");
        return;
    }

    let listed = call(&handler, "memory_list", json!({"limit": 500})).await;
    // The listing carries each memory's content, frontmatter and all, so the
    // `name:` line is what identifies it.
    let missing: Vec<&String> = names
        .iter()
        .filter(|n| !listed.contains(format!("name: {n}").as_str()))
        .collect();

    assert!(
        missing.is_empty(),
        "{} of {} memories did not survive the import: {missing:?}",
        missing.len(),
        names.len()
    );
}

/// Ask each memory's own subject and see whether it comes back.
#[tokio::test]
async fn searching_a_subject_returns_the_memory_about_it() {
    let dir = tempfile::tempdir().unwrap();
    let handler = handler(dir.path(), 5);
    let names = load_real_memories(&handler).await;
    if names.is_empty() {
        eprintln!("no memory dir on this machine; skipping");
        return;
    }

    // Subject -> the memory that should answer it.
    let probes = [
        ("what did the split cache TTL cost", "split-ttl-reverted"),
        ("how are images billed by anthropic", "features-on-but-inert"),
        ("relocation blocks billed fresh every turn", "relocated-block-billed-fresh-every-turn"),
        ("how do I restart the proxy", "headroom-proxy-restart"),
        ("what does the statusline fourth number mean", "statusline-shows-subscription-windows"),
        ("server side context editing tool clearing", "server-side-tool-clearing"),
        ("where does the proxy log live", "headroom-proxy-log-unrotated"),
        ("never mention AI in commits", "no-ai-attribution"),
    ];

    let mut hits = 0;
    let mut misses = Vec::new();
    for (query, expected) in probes {
        let found = call(&handler, "memory_search", json!({"query": query})).await;
        if found.contains(expected.replace('-', " ").as_str()) || found.contains(expected) {
            hits += 1;
        } else {
            misses.push(query);
        }
    }

    assert!(
        hits >= 7,
        "BM25 found only {hits}/8 subjects; misses: {misses:?}. \
         Recall this poor would make the memory worse than the index it replaces."
    );
}

