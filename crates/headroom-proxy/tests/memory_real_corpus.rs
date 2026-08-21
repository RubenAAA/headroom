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

/// Every config root's memory dir is a symlink into here, so this one tree
/// holds the memories of all projects and all three accounts.
const CANONICAL_DIR: &str = "/home/user/.claude-memory";
const LIVE_STORE: &str = "/home/user/.claude-personal/context-mode/memory";

/// (project slug, file) for every real memory in the canonical store.
/// `.snapshots/` holds pre-overwrite backups, and MEMORY.md is the index.
fn canonical_memory_files() -> Vec<(String, std::path::PathBuf)> {
    let mut out = Vec::new();
    let Ok(roots) = std::fs::read_dir(CANONICAL_DIR) else {
        return out;
    };
    for root in roots.flatten() {
        let slug = root.file_name().to_string_lossy().to_string();
        if slug.starts_with('.') || !root.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(root.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if path.file_name().is_some_and(|n| n == "MEMORY.md") {
                continue;
            }
            out.push((slug.clone(), path));
        }
    }
    out
}

/// Projects that have since moved. A slug names where a project *was*, and a
/// directory that no longer exists resolves to nothing, so its memories would
/// be stranded. the user supplies the new location.
const MOVED_PROJECTS: [(&str, &str); 1] = [(
    "-home-user-team-workspace-departments-analytics",
    "/home/user/meta/team-workspace/internal/analytics",
)];

/// The directory a project's sessions actually ran in.
///
/// The slug is the cwd with every separator replaced by a dash, so reversing it
/// is guesswork once a directory name contains a dash of its own. The session
/// transcripts record the real thing, so read it from there.
fn cwd_for_slug(slug: &str) -> Option<String> {
    if let Some((_, moved_to)) = MOVED_PROJECTS.iter().find(|(dead, _)| *dead == slug) {
        return Some(moved_to.to_string());
    }
    const ROOTS: [&str; 3] = [
        "/home/user/.claude",
        "/home/user/.claude-personal",
        "/home/user/.claude-work",
    ];
    for root in ROOTS {
        let dir = std::path::Path::new(root).join("projects").join(slug);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines().take(20) {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if let Some(cwd) = value.get("cwd").and_then(|c| c.as_str()) {
                    if !cwd.is_empty() {
                        return Some(cwd.to_string());
                    }
                }
            }
        }
    }
    path_from_slug(slug)
}

/// Rebuild a directory from its slug, checking each step against the disk.
///
/// The fallback for a project whose transcripts have been swept. A slug is the
/// path with every separator flattened to a dash, so `team-analytics` could be
/// one directory or two — only the filesystem can say. Longest match first,
/// and a dash that begins a segment was a dot.
fn path_from_slug(slug: &str) -> Option<String> {
    let tokens: Vec<&str> = slug.trim_start_matches('-').split('-').collect();
    let mut current = std::path::PathBuf::from("/");
    let mut i = 0;
    while i < tokens.len() {
        let (next, segment) = (i + 1..=tokens.len()).rev().find_map(|j| {
            let joined = tokens[i..j].join("-");
            let dotted = format!(".{}", joined.trim_start_matches('-'));
            for candidate in [joined, dotted] {
                if current.join(&candidate).is_dir() {
                    return Some((j, candidate));
                }
            }
            None
        })?;
        current = current.join(segment);
        i = next;
    }
    Some(current.to_string_lossy().to_string())
}

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
    call_as(handler, "default", tool, input).await
}

async fn call_as(
    handler: &MemoryHandler,
    user_id: &str,
    tool: &str,
    input: serde_json::Value,
) -> String {
    let response = json!({
        "content": [{"type": "tool_use", "id": "t1", "name": tool, "input": input}]
    });
    let results = handler
        .handle_memory_tool_calls(&response, user_id, Provider::Anthropic, None)
        .await;
    serde_json::to_string(&results).unwrap()
}

/// The partition a project's memories belong in — the same one a live request
/// from that repository resolves to.
fn partition_for_slug(slug: &str) -> Option<String> {
    let cwd = cwd_for_slug(slug)?;
    Some(headroom_proxy::memory::router::scoped_user_id(
        "default",
        &headroom_proxy::memory::router::RequestContext {
            headers: std::collections::HashMap::new(),
            system_prompt: String::new(),
            base_user_id: "default".to_string(),
            project_root_override: Some(cwd),
        },
    ))
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

/// Did this save merge into an existing memory rather than add a row?
///
/// The status arrives as JSON *inside* a tool result, so it is escaped by the
/// time it reaches here — matching the unescaped form reported "merged 0"
/// while 413 saves had in fact merged.
fn is_merge(result: &str) -> bool {
    result.contains(r#"\"status\":\"merged\""#) || result.contains(r#""status":"merged""#)
}

/// Seed the live store from every project's markdown memories.
///
/// Ignored because it writes to the real store rather than a temp dir. Run it
/// deliberately:
///
/// ```text
/// cargo test -p headroom-proxy --test memory_real_corpus -- --ignored --nocapture
/// ```
///
/// Re-running is safe: a memory that is already there restates itself, and
/// merge-on-save updates the existing row instead of adding a second one.
#[tokio::test]
#[ignore = "writes to the live store"]
async fn seed_the_live_store() {
    let dir = std::path::Path::new(LIVE_STORE);
    assert!(dir.is_dir(), "{LIVE_STORE} is not a directory");
    let handler = handler(dir, 5);

    let mut saved = 0usize;
    let mut merged = 0usize;
    let mut orphaned = Vec::new();
    let mut by_partition: std::collections::BTreeMap<String, usize> = Default::default();
    let mut partitions: std::collections::HashMap<String, Option<String>> = Default::default();

    for (slug, path) in canonical_memory_files() {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let partition = partitions
            .entry(slug.clone())
            .or_insert_with(|| partition_for_slug(&slug))
            .clone();
        let Some(user_id) = partition else {
            // No transcript left to say where this project lived, so there is
            // no partition a live request would ever resolve to. Dropping it in
            // anywhere would be worse than leaving it out.
            orphaned.push(slug);
            continue;
        };
        // The slug rides along as an entity so a memory keeps the directory it
        // came from, which the partition key alone does not record. The file's
        // frontmatter carries its name and description and the whole file is
        // the content, so nothing else has to be passed.
        let result = call_as(
            &handler,
            &user_id,
            "memory_save",
            json!({
                "content": content,
                "importance": 0.8,
                "entities": [slug],
            }),
        )
        .await;
        if is_merge(&result) {
            merged += 1;
        } else {
            saved += 1;
        }
        *by_partition.entry(user_id).or_default() += 1;
    }

    for (partition, count) in &by_partition {
        println!("{count:5}  {partition}");
    }
    println!(
        "seeded {saved}, merged {merged}, orphaned {}",
        orphaned.len()
    );
    if !orphaned.is_empty() {
        orphaned.sort();
        orphaned.dedup();
        println!("no cwd on record for: {orphaned:?}");
    }
    assert!(saved > 0, "nothing was seeded");
}

/// Same probes as the fixture test, run against the seeded live store from
/// inside the headroom partition. Ignored for the same reason as the seeder.
#[tokio::test]
#[ignore = "reads the live store"]
async fn live_store_recall() {
    let handler = handler(std::path::Path::new(LIVE_STORE), 5);
    let headroom = partition_for_slug("-home-user-headroom").expect("headroom project resolves");

    let mut hits = 0;
    let mut misses = Vec::new();
    for (query, expected) in PROBES {
        let found = call_as(
            &handler,
            &headroom,
            "memory_search",
            json!({"query": query}),
        )
        .await;
        if found.contains(expected) {
            hits += 1;
        } else {
            misses.push(query);
        }
    }
    println!(
        "headroom partition recall: {hits}/{} — misses: {misses:?}",
        PROBES.len()
    );
    assert!(hits >= 7, "recall fell to {hits}/{}", PROBES.len());
}

/// The separation itself: one project's memories must be unreachable from
/// another, whatever the query. This is the whole reason the partition exists.
#[tokio::test]
#[ignore = "reads the live store"]
async fn projects_cannot_see_each_other() {
    let handler = handler(std::path::Path::new(LIVE_STORE), 20);
    let headroom = partition_for_slug("-home-user-headroom").expect("headroom resolves");
    let shopkit = partition_for_slug("-home-user-shopkit").expect("shopkit resolves");
    assert_ne!(headroom, shopkit);

    // Ask each project a question only the *other* one can answer.
    for (partition, query, foreign) in [
        (&headroom, "react doctor bailout patterns", "react-doctor"),
        (
            &shopkit,
            "what did the split cache TTL cost",
            "split-ttl-reverted",
        ),
    ] {
        let found = call_as(
            &handler,
            partition,
            "memory_search",
            json!({"query": query}),
        )
        .await;
        assert!(
            !found.contains(foreign),
            "{partition} reached a memory belonging to another project ({foreign})"
        );
    }

    // The shared partition reaches both, and holds only user-level facts.
    for partition in [&headroom, &shopkit] {
        let shared = call_as(
            &handler,
            partition,
            "memory_search",
            json!({"query": "ANTHROPIC_API_KEY Max subscription"}),
        )
        .await;
        assert!(
            shared.contains("Max subscription"),
            "{partition} could not see the shared user-level memories"
        );
    }

    // And each still answers its own.
    let own = call_as(
        &handler,
        &headroom,
        "memory_search",
        json!({"query": "what did the split cache TTL cost"}),
    )
    .await;
    assert!(
        own.contains("split-ttl-reverted"),
        "headroom lost its own memory"
    );
}

/// Seed only the projects listed in [`MOVED_PROJECTS`].
///
/// Separate from the full seeder so a directory that moves later can be added
/// and imported without re-saving the whole corpus.
#[tokio::test]
#[ignore = "writes to the live store"]
async fn seed_relocated_projects() {
    let handler = handler(std::path::Path::new(LIVE_STORE), 5);
    for (slug, _) in MOVED_PROJECTS {
        let user_id = partition_for_slug(slug).expect("the new location resolves");
        for (file_slug, path) in canonical_memory_files() {
            if file_slug != slug {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let result = call_as(
                &handler,
                &user_id,
                "memory_save",
                json!({"content": content, "importance": 0.8, "entities": [slug]}),
            )
            .await;
            let outcome = if is_merge(&result) { "merged" } else { "saved" };
            println!(
                "{outcome}: {} -> {user_id}",
                path.file_stem().unwrap().to_string_lossy()
            );
        }
    }
}

/// Every worktree on this machine must resolve to its own repository.
///
/// The unit test builds two layouts by hand; this one asks the disk. It walks
/// the real checkouts — `shopkit/.worktrees/*`, `acme-api/.claude/worktrees/*`
/// and loose ones like `/home/user/wt-000000` that live nowhere near the
/// repository they belong to — and fails on any that would open a partition of
/// its own. Ignored because it depends on this machine's checkouts.
#[tokio::test]
#[ignore = "reads this machine's worktrees"]
async fn every_real_worktree_folds_into_its_repository() {
    let key_for = |dir: &str| {
        headroom_proxy::memory::router::scoped_user_id(
            "default",
            &headroom_proxy::memory::router::RequestContext {
                headers: std::collections::HashMap::new(),
                system_prompt: String::new(),
                base_user_id: "default".to_string(),
                project_root_override: Some(dir.to_string()),
            },
        )
    };

    let found = std::process::Command::new("find")
        .args([
            "/home/user",
            "-maxdepth",
            "6",
            "-name",
            ".git",
            "-type",
            "f",
        ])
        .output()
        .expect("find runs");
    let listing = String::from_utf8_lossy(&found.stdout);

    let mut checked = 0;
    let mut split = Vec::new();
    for git_file in listing.lines() {
        let worktree = std::path::Path::new(git_file).parent().unwrap();
        let Ok(pointer) = std::fs::read_to_string(git_file) else {
            continue;
        };
        let Some(main) = pointer
            .lines()
            .find_map(|l| l.trim().strip_prefix("gitdir:"))
            .and_then(|g| g.trim().split("/.git/worktrees/").next())
            .filter(|m| m.starts_with('/'))
        else {
            continue; // a submodule, or a pointer we cannot read
        };
        checked += 1;
        let (worktree_key, main_key) = (key_for(&worktree.to_string_lossy()), key_for(main));
        if worktree_key != main_key {
            split.push(format!(
                "{} -> {worktree_key}, want {main_key}",
                worktree.display()
            ));
        }
    }

    assert!(checked > 0, "no worktrees found to check");
    assert!(
        split.is_empty(),
        "{} of {checked} worktrees opened their own partition:\n{}",
        split.len(),
        split.join("\n")
    );
    println!("{checked} worktrees, all folded into their repository");
}

/// A repository this code has never seen needs no registration: the partition
/// is derived from the path, and the first save creates it.
#[tokio::test]
async fn a_new_repository_gets_a_partition_on_first_save() {
    let tmp = tempfile::tempdir().unwrap();
    let fresh = tmp.path().join("freshly-cloned");
    std::fs::create_dir_all(fresh.join(".git")).unwrap();
    let store = tempfile::tempdir().unwrap();
    let handler = handler(store.path(), 5);

    let user_id = headroom_proxy::memory::router::scoped_user_id(
        "default",
        &headroom_proxy::memory::router::RequestContext {
            headers: std::collections::HashMap::new(),
            system_prompt: String::new(),
            base_user_id: "default".to_string(),
            project_root_override: Some(fresh.display().to_string()),
        },
    );
    assert!(
        user_id.contains("freshly-cloned-"),
        "unexpected key {user_id}"
    );

    call_as(
        &handler,
        &user_id,
        "memory_save",
        json!({"content": "the deploy key lives in vault", "importance": 0.5}),
    )
    .await;
    let found = call_as(
        &handler,
        &user_id,
        "memory_search",
        json!({"query": "deploy key"}),
    )
    .await;
    assert!(
        found.contains("vault"),
        "a new repository could not read its own memory"
    );
}

/// A global memory reaches every project; a project memory reaches only its
/// own. The first is why the shared partition exists, the second is why it must
/// not become a back door.
#[tokio::test]
async fn a_global_memory_is_visible_from_every_project() {
    let store = tempfile::tempdir().unwrap();
    let handler = handler(store.path(), 10);
    let (alpha, beta) = ("default::alpha-1111", "default::beta-2222");

    call_as(
        &handler,
        alpha,
        "memory_save",
        json!({
            "content": "the user pays for a Max subscription; never propose an API key as a fix",
            "importance": 0.9,
            "scope": "global",
        }),
    )
    .await;
    call_as(
        &handler,
        alpha,
        "memory_save",
        json!({"content": "alpha stores its migrations under db/changes", "importance": 0.5}),
    )
    .await;

    let from_beta = call_as(
        &handler,
        beta,
        "memory_search",
        json!({"query": "API key subscription"}),
    )
    .await;
    assert!(
        from_beta.contains("Max subscription"),
        "a global memory did not reach another project"
    );

    let leaked = call_as(
        &handler,
        beta,
        "memory_search",
        json!({"query": "migrations changes"}),
    )
    .await;
    assert!(
        !leaked.contains("db/changes"),
        "a project memory leaked through the shared partition"
    );

    // And listing, which takes a different path through the backend.
    let listed = call_as(&handler, beta, "memory_list", json!({"limit": 50})).await;
    assert!(
        listed.contains("Max subscription"),
        "memory_list skipped the shared partition"
    );
    assert!(
        !listed.contains("db/changes"),
        "memory_list leaked another project"
    );
}

/// Two repositories can share a name — `ai-first` is a per-department
/// convention at meta. The path decides the partition, so they must not
/// collide however alike the names are.
#[tokio::test]
async fn repositories_with_the_same_name_do_not_collide() {
    let tmp = tempfile::tempdir().unwrap();
    let key = |dir: &std::path::Path| {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        headroom_proxy::memory::router::scoped_user_id(
            "default",
            &headroom_proxy::memory::router::RequestContext {
                headers: std::collections::HashMap::new(),
                system_prompt: String::new(),
                base_user_id: "default".to_string(),
                project_root_override: Some(dir.display().to_string()),
            },
        )
    };
    let seo = key(&tmp.path().join("seo/ai-first"));
    let analytics = key(&tmp.path().join("analytics/ai-first"));
    assert_ne!(seo, analytics, "same-named repositories shared a partition");
    assert!(seo.contains("ai-first-") && analytics.contains("ai-first-"));
}

/// Subject -> the memory that should answer it.
const PROBES: [(&str, &str); 8] = [
    ("what did the split cache TTL cost", "split-ttl-reverted"),
    (
        "how are images billed by anthropic",
        "features-on-but-inert",
    ),
    (
        "relocation blocks billed fresh every turn",
        "relocated-block-billed-fresh-every-turn",
    ),
    ("how do I restart the proxy", "headroom-proxy-restart"),
    (
        "what does the statusline fourth number mean",
        "statusline-shows-subscription-windows",
    ),
    (
        "server side context editing tool clearing",
        "server-side-tool-clearing",
    ),
    (
        "where does the proxy log live",
        "headroom-proxy-log-unrotated",
    ),
    ("never mention AI in commits", "no-ai-attribution"),
];

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

    let mut hits = 0;
    let mut misses = Vec::new();
    for (query, expected) in PROBES {
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
