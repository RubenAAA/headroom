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

/// One store is shared by every session and account, so saves collide.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_saves_all_land() {
    let dir = tempfile::tempdir().unwrap();
    let handler = Arc::new(handler(dir.path()));

    // Distinct subjects on purpose: `execute_save` deletes near-duplicates at
    // 0.92 similarity in the background, so near-identical fixtures would
    // measure the deduper rather than the concurrency.
    let subjects = [
        "the split cache TTL cost 511 percent more creation and was reverted",
        "images are billed by pixel dimensions, never by payload bytes",
        "relocation put blocks past the breakpoint and was 64 percent of the bill",
        "rust-analyzer was a rustup shim with no component installed behind it",
        "the tools array changes invalidate every live conversation at once",
        "offload digests Read results once they are four messages into history",
        "BM25 has no answer to an empty query, so listing needs enumeration",
        "Codex turns record no client bytes and skewed the savings metric",
        "context editing clears tool uses server side and is not billed",
        "the statusline fourth number is Anthropic's own window utilisation",
        "prefix divergence predicts cost better than the decline reason does",
        "subscription auth makes the Phase E normalizers passthrough entirely",
    ];
    let mut tasks = Vec::new();
    for (i, subject) in subjects.iter().enumerate() {
        let handler = Arc::clone(&handler);
        let subject = subject.to_string();
        tasks.push(tokio::spawn(async move {
            call(
                &handler,
                "memory_save",
                json!({"content": subject, "title": format!("m{i}")}),
            )
            .await
        }));
    }
    for task in tasks {
        let answer = task.await.expect("a save must not panic");
        assert!(
            answer.contains("saved"),
            "a concurrent save did not succeed: {answer}"
        );
    }

    let listed = call(&handler, "memory_list", json!({"limit": 100})).await;
    let landed = subjects.iter().filter(|s| listed.contains(*s)).count();

    assert_eq!(
        landed,
        subjects.len(),
        "every concurrent save must be readable back; got {listed}"
    );
}

/// Saving a restatement updates the original instead of adding a second row.
///
/// Nothing is deleted to achieve that — the duplicate simply never becomes a
/// row, and the caller is told which memory it landed on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_near_duplicate_merges_into_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let handler = handler(dir.path());

    let first = call(&handler, "memory_save", json!({"content": "images are billed by pixel dimensions, not bytes"})).await;
    let second = call(&handler, "memory_save", json!({"content": "images are billed by pixel dimensions, not bytes."})).await;

    assert!(second.contains("merged"), "a restatement must merge: {second}");
    assert!(
        second.contains("memory_update"),
        "and must say how to change it further: {second}"
    );

    let listed = call(&handler, "memory_list", json!({"limit": 100})).await;
    assert_eq!(
        listed.matches("pixel dimensions").count(),
        1,
        "one fact, one memory; got {listed}"
    );
    assert!(!first.contains("merged"), "the first save had nothing to merge into");
}

/// Two facts that merely share vocabulary must stay two facts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn related_but_distinct_memories_are_both_kept() {
    let dir = tempfile::tempdir().unwrap();
    let handler = handler(dir.path());

    call(&handler, "memory_save", json!({"content":
        "the split cache TTL cost 511 percent more creation on live traffic and was reverted"})).await;
    call(&handler, "memory_save", json!({"content":
        "the reminder guard cut depth-standardised cache creation by 55 percent on live traffic"})).await;

    let listed = call(&handler, "memory_list", json!({"limit": 100})).await;
    assert!(listed.contains("511 percent"), "first fact lost: {listed}");
    assert!(listed.contains("55 percent"), "second fact lost: {listed}");
}

/// Distinct subjects, because a restatement now merges rather than duplicating.
const SUBJECTS_A: [&str; 4] = [
    "the reminder guard halved depth-standardised cache creation",
    "Gemini turns take a translated route with no client bytes recorded",
    "the parity harness replays captured bodies against the python port",
    "kompress is an ONNX model on disk that has never been switched on",
];
const SUBJECTS_B: [&str; 4] = [
    "prefix divergence predicts waste better than the decline reason",
    "tool membership rather than ordering splits the cached tools block",
    "proactive expansion pasted offloaded content back into the prompt",
    "the capture-beta capture run baseline must be beaten to ship",
];

/// Two handles on one directory, standing in for two proxy processes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_stores_over_one_directory_do_not_block_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let first = Arc::new(handler(dir.path()));
    let second = Arc::new(handler(dir.path()));

    let a = {
        let first = Arc::clone(&first);
        tokio::spawn(async move {
            for i in 0..SUBJECTS_A.len() {
                call(
                    &first,
                    "memory_save",
                    json!({"content": SUBJECTS_A[i], "title": format!("a{i}")}),
                )
                .await;
            }
        })
    };
    let b = {
        let second = Arc::clone(&second);
        tokio::spawn(async move {
            for i in 0..SUBJECTS_B.len() {
                call(
                    &second,
                    "memory_save",
                    json!({"content": SUBJECTS_B[i], "title": format!("b{i}")}),
                )
                .await;
            }
        })
    };
    a.await.unwrap();
    b.await.unwrap();

    // Either handle must see both writers' memories.
    let listed = call(&second, "memory_list", json!({"limit": 100})).await;
    assert!(
        listed.contains(SUBJECTS_A[0]) && listed.contains(SUBJECTS_B[0]),
        "both processes' writes must be visible from either handle: {listed}"
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

