//! Does a memory survive being saved and come back on a search?
//!
//! The migration off Claude Code's auto-memory rests on this: 37 markdown files
//! go in through `memory_save`, and `memory_search` has to find them again in a
//! store that outlives the process. Everything else about the feature was built
//! and switched off, so the round trip had never been exercised end to end.

use std::sync::Arc;

use headroom_proxy::memory::ctx_backend::CtxMemoryBackend;
use headroom_proxy::memory::handler::{MemoryConfig, MemoryHandler, MemoryMode};
use headroom_proxy::memory::tool_adapter::Provider;
use serde_json::json;

/// A handler on a real FTS5 store in `dir`, as `proxy.rs` builds it.
fn handler(dir: &std::path::Path) -> MemoryHandler {
    let mut handler = MemoryHandler::new(
        MemoryConfig {
            enabled: true,
            backend_name: "local".to_string(),
            inject_tools: true,
            mode: MemoryMode::Tool,
            top_k: 5,
            ..Default::default()
        },
        "test",
    );
    handler.set_backend(Arc::new(
        CtxMemoryBackend::open(dir).expect("open the FTS store"),
    ));
    handler
}

/// Drive one tool call the way a real turn does: hand the handler an assistant
/// response carrying the `tool_use`, and read back what it answers with.
async fn call(handler: &MemoryHandler, tool: &str, input: serde_json::Value) -> String {
    let response = json!({
        "content": [{"type": "tool_use", "id": "t1", "name": tool, "input": input}]
    });
    assert!(
        handler.has_memory_tool_calls(&response, Provider::Anthropic),
        "the handler must recognise {tool} as its own"
    );
    let results = handler
        .handle_memory_tool_calls(&response, "default", Provider::Anthropic, None)
        .await;
    serde_json::to_string(&results).unwrap()
}

#[tokio::test]
async fn a_saved_memory_comes_back_on_a_search() {
    let dir = tempfile::tempdir().unwrap();
    let handler = handler(dir.path());

    call(
        &handler,
        "memory_save",
        json!({
            "content": "The split cache TTL cost +511% creation on live traffic and was reverted.",
            "title": "split ttl reverted",
        }),
    )
    .await;

    let found = call(&handler, "memory_search", json!({"query": "split cache TTL"})).await;

    assert!(
        found.contains("511%"),
        "a saved memory must be findable; got: {found}"
    );
}

#[tokio::test]
async fn a_memory_outlives_the_process_that_wrote_it() {
    let dir = tempfile::tempdir().unwrap();

    {
        let handler = handler(dir.path());
        call(
            &handler,
            "memory_save",
            json!({"content": "Relocation was 64% of the bill.", "title": "relocation"}),
        )
        .await;
    }

    // A second handler over the same directory, standing in for a restart.
    let reopened = handler(dir.path());
    let found = call(&reopened, "memory_search", json!({"query": "relocation"})).await;

    assert!(
        found.contains("64%"),
        "the store must survive a restart; got: {found}"
    );
}

#[tokio::test]
async fn searching_an_empty_store_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let handler = handler(dir.path());

    let found = call(&handler, "memory_search", json!({"query": "anything"})).await;

    assert!(
        !found.contains("error"),
        "an empty store should answer, not fail; got: {found}"
    );
}

#[tokio::test]
async fn a_listed_memory_is_the_one_that_was_saved() {
    let dir = tempfile::tempdir().unwrap();
    let handler = handler(dir.path());

    call(
        &handler,
        "memory_save",
        json!({"content": "Never mention AI in commits.", "title": "no ai attribution"}),
    )
    .await;

    let listed = call(&handler, "memory_list", json!({})).await;

    assert!(
        listed.contains("no ai attribution") || listed.contains("Never mention AI"),
        "list must show what was saved; got: {listed}"
    );
}
