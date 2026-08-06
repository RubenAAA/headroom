//! Per-project memory storage routing (Rust port of
//! `headroom/memory/storage_router.py`).
//!
//! Fixes the "memories bleed across projects" bug (GH #462) by giving each
//! workspace a physically isolated database file. Three storage modes:
//!
//! * `Project` (default): one DB per resolved project.
//! * `User`: one DB per `x-headroom-user-id`.
//! * `Global`: a single DB shared across everything.
//!
//! A [`BackendRouter`] owns an LRU of open backend paths keyed by on-disk
//! path so repeated requests hit a warm entry. The cache is bounded to keep
//! file-handle pressure predictable.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Known prefixes that mark a working-directory line inside a client's
/// `<env>` / `<environment>` system-prompt block. Ordered so the most
/// specific format is tried first.
const CWD_PREFIXES: &[&str] = &[
    "Primary working directory:", // Claude Code (current)
    "Working directory:",         // Claude Code (older) / Codex
    "cwd:",                       // Generic / debug format
];

/// Characters allowed in on-disk basenames.
const BASENAME_ALLOWED: &[u8] =
    b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-";

/// Physical layout for the on-disk memory store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStorageMode {
    Project,
    User,
    Global,
}

/// The slice of request state the router needs to resolve a project.
/// Built fresh per request at the provider-handler seam.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub headers: HashMap<String, String>,
    pub system_prompt: String,
    pub base_user_id: String,
    pub project_root_override: Option<String>,
}

/// The outcome of project resolution for one request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedScope {
    pub mode: MemoryStorageMode,
    pub db_path: PathBuf,
    /// Human-readable label, e.g. project basename.
    pub display_name: String,
    /// Stable hash, `None` for User/Global modes.
    pub project_key: Option<String>,
}

/// Configuration for [`BackendRouter`].
#[derive(Debug, Clone)]
pub struct BackendRouterConfig {
    pub mode: MemoryStorageMode,
    /// Filesystem root under which mode-specific subdirectories are created.
    pub root_dir: PathBuf,
    /// Path used for `Global` mode.
    pub global_db_path: PathBuf,
    /// LRU cap on simultaneously-open backends.
    pub max_open_backends: usize,
    /// Behavior when `mode` is `Project` but resolution returns `None`.
    /// `"empty"` (default): refuse to load any memory. `"global"`: fall
    /// back to Global.
    pub unresolved_project_fallback: String,
}

impl Default for BackendRouterConfig {
    fn default() -> Self {
        Self {
            mode: MemoryStorageMode::Project,
            root_dir: PathBuf::from("/tmp/headroom-memory"),
            global_db_path: PathBuf::from("/tmp/headroom-memory/memory.db"),
            max_open_backends: 16,
            unresolved_project_fallback: "empty".into(),
        }
    }
}

/// Resolve a request to a `(key, display_name)` project identity.
///
/// Looks at request signals in priority order and returns `None` when no
/// signal yields a project.
pub struct ProjectResolver;

impl ProjectResolver {
    /// Return `(project_key, display_name)` or `None`.
    pub fn resolve(&self, ctx: &RequestContext) -> Option<(String, String)> {
        // Tier 1: explicit project id header.
        if let Some(explicit) = first_nonempty_header(&ctx.headers, "x-headroom-project-id") {
            let safe = sanitize_basename(&explicit);
            if !safe.is_empty() {
                return Some((safe, explicit));
            }
        }

        // Tier 2: explicit cwd header.
        if let Some(explicit_cwd) = first_nonempty_header(&ctx.headers, "x-headroom-cwd") {
            if let Some(ident) = identity_from_cwd(&explicit_cwd) {
                return Some(ident);
            }
        }

        // Tier 3: CLI-level override.
        if let Some(ref cwd_override) = ctx.project_root_override {
            if let Some(ident) = identity_from_cwd(cwd_override) {
                return Some(ident);
            }
        }

        // Tier 4: parse system prompt for a `<env>` cwd line.
        if let Some(sys_cwd) = extract_cwd_from_system_prompt(&ctx.system_prompt) {
            if let Some(ident) = identity_from_cwd(&sys_cwd) {
                return Some(ident);
            }
        }

        None
    }
}

/// Maps a [`RequestContext`] to a resolved scope for save/search.
///
/// Holds an LRU of open backend paths so repeated traffic for the same
/// project hits a warm entry. The cache is bounded; eviction drops the
/// oldest entry.
pub struct BackendRouter {
    config: BackendRouterConfig,
    resolver: ProjectResolver,
    /// LRU cache: front = least-recently-used, back = most-recently-used.
    backends: Mutex<VecDeque<PathBuf>>,
}

impl BackendRouter {
    pub fn new(config: BackendRouterConfig) -> Self {
        Self {
            config,
            resolver: ProjectResolver,
            backends: Mutex::new(VecDeque::new()),
        }
    }

    pub fn with_resolver(config: BackendRouterConfig, resolver: ProjectResolver) -> Self {
        Self {
            config,
            resolver,
            backends: Mutex::new(VecDeque::new()),
        }
    }

    /// Resolve the scope for this request.
    pub fn resolve_scope(&self, ctx: &RequestContext) -> ResolvedScope {
        match self.config.mode {
            MemoryStorageMode::Global => ResolvedScope {
                mode: MemoryStorageMode::Global,
                db_path: self.config.global_db_path.clone(),
                display_name: "global".into(),
                project_key: None,
            },
            MemoryStorageMode::User => {
                let user_safe = sanitize_basename(&ctx.base_user_id);
                let user_dir = if user_safe.is_empty() {
                    "default".into()
                } else {
                    user_safe
                };
                let db_path = self
                    .config
                    .root_dir
                    .join("users")
                    .join(&user_dir)
                    .join("memory.db");
                ResolvedScope {
                    mode: MemoryStorageMode::User,
                    db_path,
                    display_name: ctx.base_user_id.clone(),
                    project_key: Some(user_dir),
                }
            }
            MemoryStorageMode::Project => {
                let ident = self.resolver.resolve(ctx);
                if let Some((project_key, display_name)) = ident {
                    let db_path = self
                        .config
                        .root_dir
                        .join("projects")
                        .join(&project_key)
                        .join("memory.db");
                    ResolvedScope {
                        mode: MemoryStorageMode::Project,
                        db_path,
                        display_name,
                        project_key: Some(project_key),
                    }
                } else {
                    match self.config.unresolved_project_fallback.as_str() {
                        "global" => ResolvedScope {
                            mode: MemoryStorageMode::Global,
                            db_path: self.config.global_db_path.clone(),
                            display_name: "global (unresolved)".into(),
                            project_key: None,
                        },
                        // "empty" (default) and anything else: fail-closed.
                        _ => ResolvedScope {
                            mode: MemoryStorageMode::Project,
                            db_path: self.config.global_db_path.clone(), // unused — caller checks project_key
                            display_name: "unresolved (no memory)".into(),
                            project_key: None,
                        },
                    }
                }
            }
        }
    }

    /// Acquire a backend path from the LRU cache (touch = move to back).
    /// Returns `true` if the path was already cached.
    pub fn acquire_backend(&self, db_path: &Path) -> bool {
        let mut backends = self.backends.lock().unwrap();
        if let Some(pos) = backends.iter().position(|p| p == db_path) {
            // Move to back (most-recently-used).
            let entry = backends.remove(pos).unwrap();
            backends.push_back(entry);
            true
        } else {
            backends.push_back(db_path.to_path_buf());
            // Evict LRU if over capacity.
            while backends.len() > self.config.max_open_backends {
                backends.pop_front();
            }
            false
        }
    }

    /// Snapshot of currently-cached backend paths (for tests / stats).
    pub fn open_backends(&self) -> Vec<PathBuf> {
        self.backends.lock().unwrap().iter().cloned().collect()
    }

    /// Number of currently-cached backends.
    pub fn open_count(&self) -> usize {
        self.backends.lock().unwrap().len()
    }
}

// ── free helpers ──

/// Return the first non-empty value for a header name (case-insensitive).
fn first_nonempty_header(headers: &HashMap<String, String>, name: &str) -> Option<String> {
    // Try exact match first.
    if let Some(v) = headers.get(name) {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    // Case-insensitive sweep.
    let lower = name.to_lowercase();
    for (k, v) in headers {
        if k.to_lowercase() == lower {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Extract the cwd from a system prompt by scanning for known prefixes.
fn extract_cwd_from_system_prompt(system_prompt: &str) -> Option<String> {
    if system_prompt.is_empty() {
        return None;
    }
    for prefix in CWD_PREFIXES {
        if let Some(idx) = system_prompt.find(prefix) {
            let start = idx + prefix.len();
            let end = system_prompt[start..].find('\n').map(|e| start + e);
            let chunk = match end {
                Some(e) => &system_prompt[start..e],
                None => &system_prompt[start..],
            };
            let trimmed = chunk.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Derive `(project_key, display_name)` from a raw cwd path.
fn identity_from_cwd(raw_cwd: &str) -> Option<(String, String)> {
    let cwd = raw_cwd.trim();
    if cwd.is_empty() {
        return None;
    }
    // Normalise: collapse trailing separators. We don't call realpath
    // because the proxy may run on a different machine than the client.
    let normalised = cwd.trim_end_matches('/').trim_end_matches('\\');
    let normalised = if normalised.is_empty() {
        "/"
    } else {
        normalised
    };

    let basename = Path::new(normalised)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("root");
    let safe_basename = sanitize_basename(basename);
    let safe_basename = if safe_basename.is_empty() {
        "project"
    } else {
        &safe_basename
    };

    // SHA-256 truncated to 16 hex chars for a stable, compact key.
    use sha2::{Digest, Sha256};
    let digest = {
        let mut hasher = Sha256::new();
        hasher.update(normalised.as_bytes());
        let result = hasher.finalize();
        hex_encode(&result[..8]) // 16 hex chars
    };
    let key = format!("{safe_basename}-{digest}");
    Some((key, basename.to_string()))
}

/// Sanitize a basename for filesystem use. Collapses disallowed characters
/// to `-` and trims leading/trailing `-._`.
fn sanitize_basename(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_dash = false;
    for ch in value.trim().chars() {
        if BASENAME_ALLOWED.contains(&(ch as u8)) {
            out.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    // Trim leading/trailing `-._`.
    let cleaned = out.trim_matches(|c: char| c == '-' || c == '.' || c == '_');
    // Bound length.
    let result: String = cleaned.chars().take(64).collect();
    // Return empty for empty/whitespace input — callers provide fallbacks.
    result
}

/// Encode bytes as lowercase hex.
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Best-effort extraction of the system prompt across providers.
pub fn extract_system_prompt(body: &serde_json::Value) -> String {
    // Anthropic: top-level `system` field (string or list of content blocks).
    if let Some(system) = body.get("system") {
        if let Some(s) = system.as_str() {
            return s.to_string();
        }
        if let Some(arr) = system.as_array() {
            let parts: Vec<&str> = arr
                .iter()
                .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                .collect();
            if !parts.is_empty() {
                return parts.join("\n");
            }
        }
    }

    // OpenAI/Gemini: message with role=system.
    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            if msg.get("role").and_then(|r| r.as_str()) != Some("system") {
                continue;
            }
            if let Some(content) = msg.get("content") {
                if let Some(s) = content.as_str() {
                    return s.to_string();
                }
                if let Some(arr) = content.as_array() {
                    let parts: Vec<&str> = arr
                        .iter()
                        .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                        .collect();
                    if !parts.is_empty() {
                        return parts.join("\n");
                    }
                }
            }
        }
    }

    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ProjectResolver tests ──

    #[test]
    fn resolve_explicit_project_id_header() {
        let ctx = RequestContext {
            headers: {
                let mut h = HashMap::new();
                h.insert("x-headroom-project-id".into(), "my-project".into());
                h
            },
            ..Default::default()
        };
        let resolver = ProjectResolver;
        let result = resolver.resolve(&ctx);
        assert!(result.is_some());
        let (key, display) = result.unwrap();
        assert_eq!(display, "my-project");
        assert!(!key.is_empty());
    }

    #[test]
    fn resolve_explicit_cwd_header() {
        let ctx = RequestContext {
            headers: {
                let mut h = HashMap::new();
                h.insert("x-headroom-cwd".into(), "/home/user/my-app".into());
                h
            },
            ..Default::default()
        };
        let resolver = ProjectResolver;
        let result = resolver.resolve(&ctx);
        assert!(result.is_some());
        let (key, display) = result.unwrap();
        assert!(key.starts_with("my-app-"));
        assert_eq!(display, "my-app");
    }

    #[test]
    fn resolve_project_root_override() {
        let ctx = RequestContext {
            project_root_override: Some("/opt/projects/test-repo".into()),
            ..Default::default()
        };
        let resolver = ProjectResolver;
        let result = resolver.resolve(&ctx);
        assert!(result.is_some());
        let (key, display) = result.unwrap();
        assert!(key.starts_with("test-repo-"));
        assert_eq!(display, "test-repo");
    }

    #[test]
    fn resolve_system_prompt_cwd() {
        let ctx = RequestContext {
            system_prompt: "Some preamble\nPrimary working directory: /home/bob/work\nMore text"
                .into(),
            ..Default::default()
        };
        let resolver = ProjectResolver;
        let result = resolver.resolve(&ctx);
        assert!(result.is_some());
        let (key, display) = result.unwrap();
        assert!(key.starts_with("work-"));
        assert_eq!(display, "work");
    }

    #[test]
    fn resolve_system_prompt_older_format() {
        let ctx = RequestContext {
            system_prompt: "Working directory: /tmp/test-project\n".into(),
            ..Default::default()
        };
        let resolver = ProjectResolver;
        let result = resolver.resolve(&ctx);
        assert!(result.is_some());
        let (_, display) = result.unwrap();
        assert_eq!(display, "test-project");
    }

    #[test]
    fn resolve_none_when_no_signals() {
        let ctx = RequestContext::default();
        let resolver = ProjectResolver;
        assert!(resolver.resolve(&ctx).is_none());
    }

    #[test]
    fn resolve_priority_explicit_over_cwd() {
        let ctx = RequestContext {
            headers: {
                let mut h = HashMap::new();
                h.insert("x-headroom-project-id".into(), "explicit-proj".into());
                h.insert("x-headroom-cwd".into(), "/tmp/other".into());
                h
            },
            ..Default::default()
        };
        let resolver = ProjectResolver;
        let (_, display) = resolver.resolve(&ctx).unwrap();
        assert_eq!(display, "explicit-proj");
    }

    // ── BackendRouter tests ──

    #[test]
    fn global_mode_returns_global_scope() {
        let config = BackendRouterConfig {
            mode: MemoryStorageMode::Global,
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let scope = router.resolve_scope(&RequestContext::default());
        assert_eq!(scope.mode, MemoryStorageMode::Global);
        assert_eq!(scope.display_name, "global");
        assert!(scope.project_key.is_none());
    }

    #[test]
    fn user_mode_scopes_by_user_id() {
        let config = BackendRouterConfig {
            mode: MemoryStorageMode::User,
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let ctx = RequestContext {
            base_user_id: "alice".into(),
            ..Default::default()
        };
        let scope = router.resolve_scope(&ctx);
        assert_eq!(scope.mode, MemoryStorageMode::User);
        assert_eq!(scope.display_name, "alice");
        assert!(scope.project_key.is_some());
        assert!(scope.db_path.to_string_lossy().contains("alice"));
    }

    #[test]
    fn user_mode_fallback_for_empty_user_id() {
        let config = BackendRouterConfig {
            mode: MemoryStorageMode::User,
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let scope = router.resolve_scope(&RequestContext::default());
        assert_eq!(scope.display_name, "");
        // Falls back to "default" dir.
        assert!(scope.db_path.to_string_lossy().contains("default"));
    }

    #[test]
    fn project_mode_empty_fallback_when_unresolved() {
        let config = BackendRouterConfig {
            mode: MemoryStorageMode::Project,
            unresolved_project_fallback: "empty".into(),
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let scope = router.resolve_scope(&RequestContext::default());
        assert_eq!(scope.display_name, "unresolved (no memory)");
        assert!(scope.project_key.is_none());
    }

    #[test]
    fn project_mode_global_fallback_when_unresolved() {
        let config = BackendRouterConfig {
            mode: MemoryStorageMode::Project,
            unresolved_project_fallback: "global".into(),
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let scope = router.resolve_scope(&RequestContext::default());
        assert_eq!(scope.mode, MemoryStorageMode::Global);
        assert_eq!(scope.display_name, "global (unresolved)");
        assert!(scope.project_key.is_none());
    }

    #[test]
    fn project_mode_resolved_from_header() {
        let config = BackendRouterConfig {
            mode: MemoryStorageMode::Project,
            root_dir: PathBuf::from("/data/memory"),
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let ctx = RequestContext {
            headers: {
                let mut h = HashMap::new();
                h.insert("x-headroom-project-id".into(), "my-proj".into());
                h
            },
            ..Default::default()
        };
        let scope = router.resolve_scope(&ctx);
        assert_eq!(scope.mode, MemoryStorageMode::Project);
        assert_eq!(scope.display_name, "my-proj");
        assert!(scope.project_key.is_some());
        assert!(scope.db_path.to_string_lossy().contains("projects"));
    }

    // ── LRU cache tests ──

    #[test]
    fn acquire_backend_caches_path() {
        let router = BackendRouter::new(BackendRouterConfig::default());
        let path = PathBuf::from("/tmp/test.db");
        assert!(!router.acquire_backend(&path)); // not cached
        assert!(router.acquire_backend(&path)); // now cached
        assert_eq!(router.open_count(), 1);
    }

    #[test]
    fn acquire_backend_lru_eviction() {
        let config = BackendRouterConfig {
            max_open_backends: 2,
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let p1 = PathBuf::from("/tmp/1.db");
        let p2 = PathBuf::from("/tmp/2.db");
        let p3 = PathBuf::from("/tmp/3.db");

        router.acquire_backend(&p1);
        router.acquire_backend(&p2);
        assert_eq!(router.open_count(), 2);

        // Adding p3 evicts p1 (LRU).
        router.acquire_backend(&p3);
        assert_eq!(router.open_count(), 2);
        let open = router.open_backends();
        assert!(!open.contains(&p1));
        assert!(open.contains(&p2));
        assert!(open.contains(&p3));
    }

    #[test]
    fn acquire_backend_touch_moves_to_back() {
        let config = BackendRouterConfig {
            max_open_backends: 2,
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let p1 = PathBuf::from("/tmp/1.db");
        let p2 = PathBuf::from("/tmp/2.db");

        router.acquire_backend(&p1);
        router.acquire_backend(&p2);
        // Touch p1 — now p2 is LRU.
        router.acquire_backend(&p1);
        // Adding p3 evicts p2.
        let p3 = PathBuf::from("/tmp/3.db");
        router.acquire_backend(&p3);
        let open = router.open_backends();
        assert!(open.contains(&p1)); // p1 was touched, survived
        assert!(!open.contains(&p2)); // p2 was LRU, evicted
    }

    // ── sanitize_basename tests ──

    #[test]
    fn sanitize_basename_normal() {
        assert_eq!(sanitize_basename("my-project"), "my-project");
        assert_eq!(sanitize_basename("test_repo.v2"), "test_repo.v2");
    }

    #[test]
    fn sanitize_basename_collapses_special_chars() {
        assert_eq!(sanitize_basename("hello world!"), "hello-world");
        assert_eq!(sanitize_basename("a@b#c"), "a-b-c");
    }

    #[test]
    fn sanitize_basename_trims_dashes_dots_underscores() {
        assert_eq!(sanitize_basename("--foo--"), "foo");
        assert_eq!(sanitize_basename("._bar_."), "bar");
    }

    #[test]
    fn sanitize_basename_truncates_long_names() {
        let long = "a".repeat(100);
        assert_eq!(sanitize_basename(&long).len(), 64);
    }

    #[test]
    fn sanitize_basename_empty_returns_empty() {
        assert_eq!(sanitize_basename("   "), "");
        assert_eq!(sanitize_basename("---"), "");
    }

    // ── extract_cwd_from_system_prompt tests ──

    #[test]
    fn extract_cwd_primary_working_directory() {
        let prompt = "Some text\nPrimary working directory: /home/alice/proj\nMore text";
        assert_eq!(
            extract_cwd_from_system_prompt(prompt).as_deref(),
            Some("/home/alice/proj")
        );
    }

    #[test]
    fn extract_cwd_working_directory() {
        let prompt = "Working directory: /tmp/test\n";
        assert_eq!(
            extract_cwd_from_system_prompt(prompt).as_deref(),
            Some("/tmp/test")
        );
    }

    #[test]
    fn extract_cwd_cwd_prefix() {
        let prompt = "cwd: /opt/app\n";
        assert_eq!(
            extract_cwd_from_system_prompt(prompt).as_deref(),
            Some("/opt/app")
        );
    }

    #[test]
    fn extract_cwd_none_when_empty() {
        assert!(extract_cwd_from_system_prompt("").is_none());
        assert!(extract_cwd_from_system_prompt("no cwd here").is_none());
    }

    #[test]
    fn extract_cwd_no_trailing_newline() {
        let prompt = "Primary working directory: /end/of/prompt";
        assert_eq!(
            extract_cwd_from_system_prompt(prompt).as_deref(),
            Some("/end/of/prompt")
        );
    }

    // ── extract_system_prompt tests ──

    #[test]
    fn extract_system_prompt_anthropic_string() {
        let body = serde_json::json!({
            "system": "You are helpful.",
            "messages": []
        });
        assert_eq!(extract_system_prompt(&body), "You are helpful.");
    }

    #[test]
    fn extract_system_prompt_anthropic_list() {
        let body = serde_json::json!({
            "system": [
                {"type": "text", "text": "Part 1"},
                {"type": "text", "text": "Part 2"}
            ]
        });
        assert_eq!(extract_system_prompt(&body), "Part 1\nPart 2");
    }

    #[test]
    fn extract_system_prompt_openai_message() {
        let body = serde_json::json!({
            "messages": [
                {"role": "system", "content": "Be concise."},
                {"role": "user", "content": "Hello"}
            ]
        });
        assert_eq!(extract_system_prompt(&body), "Be concise.");
    }

    #[test]
    fn extract_system_prompt_none_when_absent() {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "Hello"}]
        });
        assert_eq!(extract_system_prompt(&body), "");
    }

    // ── identity_from_cwd tests ──

    #[test]
    fn identity_from_cwd_basic() {
        let (key, display) = identity_from_cwd("/home/user/my-app").unwrap();
        assert!(key.starts_with("my-app-"));
        assert_eq!(display, "my-app");
    }

    #[test]
    fn identity_from_cwd_trailing_slash() {
        let (key1, _) = identity_from_cwd("/home/user/my-app").unwrap();
        let (key2, _) = identity_from_cwd("/home/user/my-app/").unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn identity_from_cwd_empty() {
        assert!(identity_from_cwd("").is_none());
        assert!(identity_from_cwd("   ").is_none());
    }

    #[test]
    fn identity_from_cwd_root() {
        let (key, display) = identity_from_cwd("/").unwrap();
        assert!(key.starts_with("root-"));
        assert_eq!(display, "root");
    }
}
