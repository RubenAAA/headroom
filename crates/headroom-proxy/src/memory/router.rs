//! Per-project memory storage routing.
//!
//! Fixes cross-project memory bleed by giving each workspace a physically
//! isolated SQLite database. Three modes: PROJECT, USER, GLOBAL.
//!
//! Mirrors Python's `headroom.memory.storage_router`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

// ─── Storage mode ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryStorageMode {
    Project,
    User,
    Global,
}

impl std::fmt::Display for MemoryStorageMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryStorageMode::Project => write!(f, "project"),
            MemoryStorageMode::User => write!(f, "user"),
            MemoryStorageMode::Global => write!(f, "global"),
        }
    }
}

// ─── Request context ─────────────────────────────────────────────────────

/// The slice of request state the router needs to resolve a project.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub headers: HashMap<String, String>,
    pub system_prompt: String,
    pub base_user_id: String,
    pub project_root_override: Option<String>,
}

// ─── Resolved scope ──────────────────────────────────────────────────────

/// Outcome of project resolution for one request.
#[derive(Debug, Clone)]
pub struct ResolvedScope {
    pub mode: MemoryStorageMode,
    pub db_path: PathBuf,
    pub display_name: String,
    pub project_key: Option<String>,
}

// ─── Router config ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BackendRouterConfig {
    pub mode: MemoryStorageMode,
    pub root_dir: PathBuf,
    pub global_db_path: PathBuf,
    pub max_open_backends: usize,
    pub unresolved_project_fallback: String,
}

impl Default for BackendRouterConfig {
    fn default() -> Self {
        Self {
            mode: MemoryStorageMode::Project,
            root_dir: PathBuf::from("memories"),
            global_db_path: PathBuf::from("memory.db"),
            max_open_backends: 16,
            unresolved_project_fallback: "empty".to_string(),
        }
    }
}

// ─── Project resolver ────────────────────────────────────────────────────

/// Known CWD prefixes in client system prompts.
const CWD_PREFIXES: &[&str] = &["Primary working directory:", "Working directory:", "cwd:"];

/// Characters allowed in on-disk basenames.
fn is_basename_allowed(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-'
}

/// Resolve a request to a (key, display_name) project identity.
pub struct ProjectResolver;

impl ProjectResolver {
    /// Return `(project_key, display_name)` or None.
    pub fn resolve(ctx: &RequestContext) -> Option<(String, String)> {
        // Tier 1: explicit project id header
        if let Some(explicit) = Self::first_nonempty_header(&ctx.headers, "x-headroom-project-id") {
            let safe = Self::sanitize_basename(&explicit);
            if !safe.is_empty() {
                return Some((safe, explicit));
            }
        }

        // Tier 2: explicit cwd header
        if let Some(cwd) = Self::first_nonempty_header(&ctx.headers, "x-headroom-cwd") {
            if let Some(ident) = Self::identity_from_cwd(&cwd) {
                return Some(ident);
            }
        }

        // Tier 3: CLI override
        if let Some(ref override_root) = ctx.project_root_override {
            if let Some(ident) = Self::identity_from_cwd(override_root) {
                return Some(ident);
            }
        }

        // Tier 4: parse system prompt for cwd
        if let Some(sys_cwd) = Self::extract_cwd_from_system_prompt(&ctx.system_prompt) {
            if let Some(ident) = Self::identity_from_cwd(&sys_cwd) {
                return Some(ident);
            }
        }

        None
    }

    fn first_nonempty_header(headers: &HashMap<String, String>, name: &str) -> Option<String> {
        // Try exact match first
        if let Some(v) = headers.get(name) {
            let trimmed = v.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
        // Case-insensitive sweep
        let lower = name.to_lowercase();
        for (k, v) in headers {
            if k.to_lowercase() == lower {
                let trimmed = v.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
        None
    }

    fn extract_cwd_from_system_prompt(system_prompt: &str) -> Option<String> {
        if system_prompt.is_empty() {
            return None;
        }
        for prefix in CWD_PREFIXES {
            if let Some(idx) = system_prompt.find(prefix) {
                let start = idx + prefix.len();
                let end = system_prompt[start..].find('\n');
                let chunk = match end {
                    Some(e) => &system_prompt[start..start + e],
                    None => &system_prompt[start..],
                };
                let trimmed = chunk.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
        None
    }

    fn identity_from_cwd(raw_cwd: &str) -> Option<(String, String)> {
        let cwd = raw_cwd.trim();
        if cwd.is_empty() {
            return None;
        }
        // Normalise path (best-effort)
        let normalised = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
        let normalised_str = normalised
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        let normalised_str = if normalised_str.is_empty() {
            "/".to_string()
        } else {
            normalised_str
        };
        let basename = normalised
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".to_string());
        let safe_basename = Self::sanitize_basename(&basename);
        let safe_basename = if safe_basename.is_empty() {
            "project".to_string()
        } else {
            safe_basename
        };
        let digest = sha256_hex(normalised_str.as_bytes());
        let key = format!("{}-{}", &safe_basename, &digest[..16]);
        Some((key, basename))
    }

    pub fn sanitize_basename(value: &str) -> String {
        let mut out = Vec::new();
        let mut last_was_dash = false;
        for ch in value.trim().chars() {
            if is_basename_allowed(ch) {
                out.push(ch);
                last_was_dash = false;
            } else if !last_was_dash {
                out.push('-');
                last_was_dash = true;
            }
        }
        let cleaned: String = out.into_iter().collect();
        let cleaned = cleaned.trim_matches(|c| c == '-' || c == '.' || c == '_');
        // Bound length
        cleaned.chars().take(64).collect()
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex::encode(result)
}

// ─── Backend router ──────────────────────────────────────────────────────

/// Maps a RequestContext to a backend path. Holds an LRU of open backend paths.
pub struct BackendRouter {
    config: BackendRouterConfig,
    // LRU cache: db_path → position. We store PathBuf keys in order.
    backends: Mutex<Vec<PathBuf>>,
}

impl BackendRouter {
    pub fn new(config: BackendRouterConfig) -> Self {
        Self {
            config,
            backends: Mutex::new(Vec::new()),
        }
    }

    /// Resolve a request to a scope (backend path + metadata).
    pub fn resolve_scope(&self, ctx: &RequestContext) -> ResolvedScope {
        match self.config.mode {
            MemoryStorageMode::Global => ResolvedScope {
                mode: MemoryStorageMode::Global,
                db_path: self.config.global_db_path.clone(),
                display_name: "global".to_string(),
                project_key: None,
            },

            MemoryStorageMode::User => {
                let user_safe = ProjectResolver::sanitize_basename(&ctx.base_user_id);
                let user_safe = if user_safe.is_empty() {
                    "default".to_string()
                } else {
                    user_safe
                };
                let db_path = self
                    .config
                    .root_dir
                    .join("users")
                    .join(&user_safe)
                    .join("memory.db");
                ResolvedScope {
                    mode: MemoryStorageMode::User,
                    db_path,
                    display_name: ctx.base_user_id.clone(),
                    project_key: Some(user_safe),
                }
            }

            MemoryStorageMode::Project => {
                let ident = ProjectResolver::resolve(ctx);
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
                    // Unresolved — apply fallback
                    match self.config.unresolved_project_fallback.as_str() {
                        "global" => ResolvedScope {
                            mode: MemoryStorageMode::Global,
                            db_path: self.config.global_db_path.clone(),
                            display_name: "global (unresolved)".to_string(),
                            project_key: None,
                        },
                        _ => {
                            // "empty" or unknown — fail-closed
                            ResolvedScope {
                                mode: MemoryStorageMode::Project,
                                db_path: self.config.global_db_path.clone(), // Unused
                                display_name: "unresolved (no memory)".to_string(),
                                project_key: None,
                            }
                        }
                    }
                }
            }
        }
    }

    /// Track a backend path as recently used (LRU touch).
    pub fn touch(&self, path: &Path) {
        let mut backends = self.backends.lock().unwrap_or_else(|e| e.into_inner());
        backends.retain(|p| p != path);
        backends.push(path.to_path_buf());
        // Evict oldest if over limit
        while backends.len() > self.config.max_open_backends {
            backends.remove(0);
        }
    }

    /// Get snapshot of currently-tracked backend paths.
    pub fn open_backends(&self) -> Vec<PathBuf> {
        self.backends
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

// ─── System prompt extraction ────────────────────────────────────────────

/// Best-effort extraction of the system prompt across providers.
pub fn extract_system_prompt(body: &Value) -> String {
    // Anthropic: top-level "system" field
    if let Some(system) = body.get("system") {
        if let Some(s) = system.as_str() {
            return s.to_string();
        }
        if let Some(arr) = system.as_array() {
            let parts: Vec<&str> = arr
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            if !parts.is_empty() {
                return parts.join("\n");
            }
        }
    }

    // OpenAI/Gemini: role=system message
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for msg in messages {
            if msg.get("role").and_then(Value::as_str) != Some("system") {
                continue;
            }
            if let Some(content) = msg.get("content") {
                if let Some(s) = content.as_str() {
                    return s.to_string();
                }
                if let Some(arr) = content.as_array() {
                    let parts: Vec<&str> = arr
                        .iter()
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
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

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- ProjectResolver ---

    #[test]
    fn resolve_explicit_project_id() {
        let ctx = RequestContext {
            headers: HashMap::from([("x-headroom-project-id".to_string(), "my-proj".to_string())]),
            system_prompt: String::new(),
            base_user_id: "u1".to_string(),
            project_root_override: None,
        };
        let result = ProjectResolver::resolve(&ctx);
        assert!(result.is_some());
        let (key, name) = result.unwrap();
        assert_eq!(name, "my-proj");
        // Tier 1 returns the sanitized basename directly (no hash prefix).
        assert_eq!(key, "my-proj");
    }

    #[test]
    fn resolve_explicit_cwd() {
        let ctx = RequestContext {
            headers: HashMap::from([(
                "x-headroom-cwd".to_string(),
                "/home/user/project".to_string(),
            )]),
            system_prompt: String::new(),
            base_user_id: "u1".to_string(),
            project_root_override: None,
        };
        let result = ProjectResolver::resolve(&ctx);
        assert!(result.is_some());
        let (key, name) = result.unwrap();
        assert_eq!(name, "project");
        assert!(key.starts_with("project-"));
    }

    #[test]
    fn resolve_system_prompt_cwd() {
        let ctx = RequestContext {
            headers: HashMap::new(),
            system_prompt: "Some instructions\nWorking directory: /tmp/myapp\nMore text"
                .to_string(),
            base_user_id: "u1".to_string(),
            project_root_override: None,
        };
        let result = ProjectResolver::resolve(&ctx);
        assert!(result.is_some());
        let (_, name) = result.unwrap();
        assert_eq!(name, "myapp");
    }

    #[test]
    fn resolve_none_when_no_signals() {
        let ctx = RequestContext {
            headers: HashMap::new(),
            system_prompt: String::new(),
            base_user_id: "u1".to_string(),
            project_root_override: None,
        };
        assert!(ProjectResolver::resolve(&ctx).is_none());
    }

    #[test]
    fn resolve_primary_working_directory_prefix() {
        let ctx = RequestContext {
            headers: HashMap::new(),
            system_prompt: "Primary working directory: /workspace/code".to_string(),
            base_user_id: "u1".to_string(),
            project_root_override: None,
        };
        let result = ProjectResolver::resolve(&ctx);
        assert!(result.is_some());
        let (_, name) = result.unwrap();
        assert_eq!(name, "code");
    }

    // --- sanitize_basename ---

    #[test]
    fn sanitize_normal() {
        assert_eq!(
            ProjectResolver::sanitize_basename("my-project"),
            "my-project"
        );
    }

    #[test]
    fn sanitize_special_chars() {
        assert_eq!(
            ProjectResolver::sanitize_basename("my project!"),
            "my-project"
        );
    }

    #[test]
    fn sanitize_trims_dashes() {
        assert_eq!(ProjectResolver::sanitize_basename("---hello---"), "hello");
    }

    #[test]
    fn sanitize_max_length() {
        let long = "a".repeat(100);
        assert_eq!(ProjectResolver::sanitize_basename(&long).len(), 64);
    }

    // --- extract_cwd_from_system_prompt ---

    #[test]
    fn extract_cwd_working_dir() {
        let prompt = "Instructions\nWorking directory: /home/user/code\nEnd";
        assert_eq!(
            ProjectResolver::extract_cwd_from_system_prompt(prompt).as_deref(),
            Some("/home/user/code")
        );
    }

    #[test]
    fn extract_cwd_cwd_prefix() {
        let prompt = "cwd: /tmp/test\n";
        assert_eq!(
            ProjectResolver::extract_cwd_from_system_prompt(prompt).as_deref(),
            Some("/tmp/test")
        );
    }

    #[test]
    fn extract_cwd_none() {
        assert!(ProjectResolver::extract_cwd_from_system_prompt("no cwd here").is_none());
    }

    #[test]
    fn extract_cwd_empty() {
        assert!(ProjectResolver::extract_cwd_from_system_prompt("").is_none());
    }

    // --- BackendRouter ---

    #[test]
    fn router_global_mode() {
        let config = BackendRouterConfig {
            mode: MemoryStorageMode::Global,
            global_db_path: PathBuf::from("/data/memory.db"),
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let ctx = RequestContext {
            headers: HashMap::new(),
            system_prompt: String::new(),
            base_user_id: "u1".to_string(),
            project_root_override: None,
        };
        let scope = router.resolve_scope(&ctx);
        assert_eq!(scope.mode, MemoryStorageMode::Global);
        assert_eq!(scope.db_path, PathBuf::from("/data/memory.db"));
        assert!(scope.project_key.is_none());
    }

    #[test]
    fn router_user_mode() {
        let config = BackendRouterConfig {
            mode: MemoryStorageMode::User,
            root_dir: PathBuf::from("/data"),
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let ctx = RequestContext {
            headers: HashMap::new(),
            system_prompt: String::new(),
            base_user_id: "alice".to_string(),
            project_root_override: None,
        };
        let scope = router.resolve_scope(&ctx);
        assert_eq!(scope.mode, MemoryStorageMode::User);
        assert!(scope.db_path.to_string_lossy().contains("alice"));
    }

    #[test]
    fn router_project_mode_resolved() {
        let config = BackendRouterConfig {
            mode: MemoryStorageMode::Project,
            root_dir: PathBuf::from("/data"),
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let ctx = RequestContext {
            headers: HashMap::from([("x-headroom-project-id".to_string(), "proj1".to_string())]),
            system_prompt: String::new(),
            base_user_id: "u1".to_string(),
            project_root_override: None,
        };
        let scope = router.resolve_scope(&ctx);
        assert_eq!(scope.mode, MemoryStorageMode::Project);
        assert!(scope.project_key.is_some());
        assert!(scope.db_path.to_string_lossy().contains("proj1"));
    }

    #[test]
    fn router_project_mode_unresolved_empty() {
        let config = BackendRouterConfig {
            mode: MemoryStorageMode::Project,
            unresolved_project_fallback: "empty".to_string(),
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let ctx = RequestContext {
            headers: HashMap::new(),
            system_prompt: String::new(),
            base_user_id: "u1".to_string(),
            project_root_override: None,
        };
        let scope = router.resolve_scope(&ctx);
        assert!(scope.project_key.is_none());
        assert_eq!(scope.display_name, "unresolved (no memory)");
    }

    #[test]
    fn router_project_mode_unresolved_global() {
        let config = BackendRouterConfig {
            mode: MemoryStorageMode::Project,
            unresolved_project_fallback: "global".to_string(),
            global_db_path: PathBuf::from("/data/global.db"),
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        let ctx = RequestContext {
            headers: HashMap::new(),
            system_prompt: String::new(),
            base_user_id: "u1".to_string(),
            project_root_override: None,
        };
        let scope = router.resolve_scope(&ctx);
        assert_eq!(scope.mode, MemoryStorageMode::Global);
        assert_eq!(scope.db_path, PathBuf::from("/data/global.db"));
    }

    #[test]
    fn router_lru_touch_and_evict() {
        let config = BackendRouterConfig {
            max_open_backends: 2,
            ..Default::default()
        };
        let router = BackendRouter::new(config);
        router.touch(&Path::new("/a"));
        router.touch(&Path::new("/b"));
        router.touch(&Path::new("/c"));
        let open = router.open_backends();
        assert_eq!(open.len(), 2);
        assert!(!open.contains(&PathBuf::from("/a"))); // evicted
        assert!(open.contains(&PathBuf::from("/b")));
        assert!(open.contains(&PathBuf::from("/c")));
    }

    // --- extract_system_prompt ---

    #[test]
    fn extract_system_anthropic_string() {
        let body = json!({"system": "You are a helpful assistant."});
        assert_eq!(extract_system_prompt(&body), "You are a helpful assistant.");
    }

    #[test]
    fn extract_system_anthropic_blocks() {
        let body = json!({"system": [{"type": "text", "text": "Part 1"}, {"type": "text", "text": "Part 2"}]});
        assert_eq!(extract_system_prompt(&body), "Part 1\nPart 2");
    }

    #[test]
    fn extract_system_openai_message() {
        let body = json!({"messages": [{"role": "system", "content": "Be helpful."}]});
        assert_eq!(extract_system_prompt(&body), "Be helpful.");
    }

    #[test]
    fn extract_system_none() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert!(extract_system_prompt(&body).is_empty());
    }

    #[test]
    fn extract_system_empty_body() {
        assert!(extract_system_prompt(&json!({})).is_empty());
    }
}
