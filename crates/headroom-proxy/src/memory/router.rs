//! Per-project memory storage routing.
//!
//! Fixes cross-project memory bleed by giving each workspace a physically
//! isolated SQLite database. Three modes: PROJECT, USER, GLOBAL.
//!
//! Mirrors Python's `headroom.memory.storage_router`.

use std::collections::HashMap;
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

/// The user id a request's memories live under.
///
/// One store serves every project and every account, and the backend partitions
/// only by `user_id` — so the project has to go in it. Without this a shopkit
/// memory can answer a headroom question, which is the one thing separate
/// project memories exist to prevent.
///
/// Falls back to the bare user id when no project resolves. That is a shared
/// pool, which is worse than separation, but it is better than dropping the
/// write into a partition nothing will ever search again.
pub fn scoped_user_id(base_user_id: &str, ctx: &RequestContext) -> String {
    match ProjectResolver::resolve(ctx) {
        Some((key, _display)) => format!("{base_user_id}{PROJECT_SEPARATOR}{key}"),
        None => base_user_id.to_string(),
    }
}

/// Separates the user id from the project inside a partition key.
const PROJECT_SEPARATOR: &str = "::";

/// The partition shared by every project for this user.
///
/// Not everything worth remembering belongs to a repository. "Never suggest an
/// API key, this is a Max subscription" and "prefers pytest in Docker" are
/// facts about the person, and filing them under whichever directory was open
/// at the time hides them everywhere else. Searches read this alongside the
/// project's own partition; only a deliberate global save writes to it.
pub fn shared_partition(scoped_user_id: &str) -> &str {
    match scoped_user_id.split_once(PROJECT_SEPARATOR) {
        Some((base, _project)) => base,
        None => scoped_user_id,
    }
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
        if let Some(cwd) = Self::first_nonempty_header(&ctx.headers, "x-headroom-cwd")
            && let Some(ident) = Self::identity_from_cwd(&cwd)
        {
            return Some(ident);
        }

        // Tier 3: parse system prompt for cwd
        if let Some(sys_cwd) = Self::extract_cwd_from_system_prompt(&ctx.system_prompt)
            && let Some(ident) = Self::identity_from_cwd(&sys_cwd)
        {
            return Some(ident);
        }

        // Tier 4: CLI override, last resort. `--memory-project-root` documents
        // itself as catching requests that carry *no* cwd metadata ("no header,
        // no system-prompt cwd"), so it has to sit below the system-prompt tier.
        // Above it, one operator setting would capture every project's turns —
        // the cross-project bleed this module exists to prevent — because only
        // a request with an explicit header could escape it.
        if let Some(ref override_root) = ctx.project_root_override
            && let Some(ident) = Self::identity_from_cwd(override_root)
        {
            return Some(ident);
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

    /// The project a request's ctx stores belong to, or `None` when nothing in
    /// the request names one.
    ///
    /// Same tier order as [`Self::resolve`], and the same project: the
    /// repository's origin, or its root path when it has none. The ctx stores
    /// hash this value into their file names (`hash_project_dir_canonical`), so
    /// a subdirectory and a worktree share their repository's stores, and a
    /// moved checkout keeps them.
    pub fn resolve_project_dir(ctx: &RequestContext) -> Option<String> {
        // An explicit project id is not a path. It is still a stable bucket of
        // its own, which is what sharding needs.
        if let Some(explicit) = Self::first_nonempty_header(&ctx.headers, "x-headroom-project-id") {
            let safe = Self::sanitize_basename(&explicit);
            if !safe.is_empty() {
                return Some(safe);
            }
        }
        [
            Self::first_nonempty_header(&ctx.headers, "x-headroom-cwd"),
            Self::extract_cwd_from_system_prompt(&ctx.system_prompt),
            ctx.project_root_override.clone(),
        ]
        .into_iter()
        .flatten()
        .find_map(|raw| Self::project_identity(&raw).map(|(identity, _name)| identity))
    }

    /// Canonicalize a cwd: resolve symlinks where the path exists, drop the
    /// trailing slash. Best-effort — a directory this machine cannot see still
    /// shards consistently by its literal form.
    fn normalize_cwd(raw_cwd: &str) -> Option<String> {
        let cwd = raw_cwd.trim();
        if cwd.is_empty() {
            return None;
        }
        let normalised = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
        let s = normalised
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        Some(if s.is_empty() { "/".to_string() } else { s })
    }

    fn identity_from_cwd(raw_cwd: &str) -> Option<(String, String)> {
        let (identity, name) = Self::project_identity(raw_cwd)?;
        let safe_name = Self::sanitize_basename(&name);
        let safe_name = if safe_name.is_empty() {
            "project".to_string()
        } else {
            safe_name
        };
        let digest = sha256_hex(identity.as_bytes());
        Some((format!("{}-{}", safe_name, &digest[..16]), name))
    }

    /// What project a directory belongs to, as `(identity, display name)`.
    ///
    /// The repository, not the directory the session happened to start in, and
    /// keyed on where the repository came from so a checkout keeps its
    /// project when it is moved or cloned again somewhere else. A repository
    /// with no `origin`, or no repository at all, is its root path.
    fn project_identity(raw_cwd: &str) -> Option<(String, String)> {
        let normalised_str = Self::normalize_cwd(raw_cwd)?;
        let root = Self::repo_root(&PathBuf::from(&normalised_str));
        if let Some(remote) = Self::origin_remote(&root) {
            let name = remote.rsplit('/').next().unwrap_or(&remote).to_string();
            return Some((remote, name));
        }
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".to_string());
        Some((root.to_string_lossy().to_string(), name))
    }

    /// Walk up to the repository a directory belongs to.
    ///
    /// Keyed on the raw cwd, every worktree and every subdirectory would be its
    /// own project: `shopkit`, `shopkit/apps/api` and
    /// `shopkit/.claude/worktrees/access-gates` would each keep a separate set of
    /// memories for one codebase. A linked worktree's `.git` is a *file*
    /// pointing into `<main>/.git/worktrees/<name>`, which is what lets them
    /// fold back onto the repository they came from.
    ///
    /// Returns the directory unchanged when nothing above it is a repository —
    /// a session in a plain directory still gets its own pool.
    fn repo_root(dir: &Path) -> PathBuf {
        let mut cursor = Some(dir);
        while let Some(path) = cursor {
            let git = path.join(".git");
            // A directory named `.git` is not a repository; one containing a
            // `HEAD` is. Accepting the bare name let any stray or half-removed
            // `.git` above a project re-root it -- an empty `/tmp/.git` on this
            // machine made every temp-dir project resolve to `tmp`, which is
            // two different projects sharing one memory partition. The `.git`
            // *file* case below needs no such check: it is only accepted when
            // its `gitdir:` pointer parses.
            if git.is_dir() && git.join("HEAD").is_file() {
                return path.to_path_buf();
            }
            if git.is_file() {
                return Self::main_repo_from_git_file(&git).unwrap_or_else(|| path.to_path_buf());
            }
            cursor = path.parent();
        }
        dir.to_path_buf()
    }

    /// The `origin` URL of the repository at `root`, reduced to `host/owner/name`
    /// so an https clone and an ssh clone of one repository agree.
    ///
    /// `None` sends the caller back to the path: no repository, no `origin`, a
    /// relative URL (it means something different from every directory), or a
    /// submodule, whose `.git` is a file and whose config lives elsewhere.
    fn origin_remote(root: &Path) -> Option<String> {
        let config = std::fs::read_to_string(root.join(".git").join("config")).ok()?;
        let mut in_origin = false;
        let mut url = None;
        for line in config.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_origin = line == r#"[remote "origin"]"#;
            } else if in_origin
                && let Some((k, v)) = line.split_once('=')
                && k.trim() == "url"
            {
                url = Some(v.trim().to_string());
            }
        }
        let url = url?;
        if url.is_empty() || url.starts_with('.') {
            return None;
        }
        let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
        let rest = match rest.split_once('@') {
            Some((user, r)) if !user.contains('/') => r,
            _ => rest,
        };
        // scp-like `host:owner/name`; a scheme URL has already lost its colon
        // unless it names a port, which this leaves alone.
        let rest = if !url.contains("://") {
            rest.replacen(':', "/", 1)
        } else {
            rest.to_string()
        };
        let rest = rest.trim_end_matches('/');
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        Some(rest.to_lowercase())
    }

    /// The main repository a linked worktree points at, if that is what this is.
    ///
    /// Where the worktree sits does not matter — `/home/user/wt-000000` lives
    /// nowhere near the repository it belongs to, and still names it here.
    fn main_repo_from_git_file(git_file: &Path) -> Option<PathBuf> {
        const WORKTREE_MARKER: &str = "/.git/worktrees/";
        let text = std::fs::read_to_string(git_file).ok()?;
        let pointer = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("gitdir:"))?
            .trim();
        // Git writes an absolute pointer by default but a relative one under
        // `worktree.useRelativePaths`, and that is relative to the worktree.
        let gitdir = if pointer.starts_with('/') {
            PathBuf::from(pointer)
        } else {
            git_file.parent()?.join(pointer)
        };
        let gitdir = gitdir.to_string_lossy();
        // A submodule's pointer lands in `<super>/.git/modules/<name>` instead,
        // and a submodule really is its own project. Last occurrence, so a
        // repository that itself lives inside someone else's worktree still
        // resolves to itself.
        let idx = gitdir.rfind(WORKTREE_MARKER)?;
        Some(PathBuf::from(&gitdir[..idx]))
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

/// The system prompt, led by the working directory the client stated last.
///
/// Claude Code no longer states its directory in `system`: it arrives in the
/// opening user message, and a later `cd` arrives as a `role: "system"`
/// message reading `Primary working directory: /new (was /old)`. Read from
/// `system` alone, every request fell through to `--memory-project-root`, so
/// every repository shared that one project's memories. The statement goes
/// first because the resolver takes the first line it finds.
///
/// Memory reads this one. It is injected at the tail, so it can follow a `cd`
/// without touching the cached prefix.
pub fn extract_project_prompt(body: &Value) -> String {
    lead_with_cwd(stated_cwds(body).pop(), body)
}

/// The system prompt, led by the working directory the conversation opened in.
///
/// The ctx stores read this one. Recall is decided once per conversation,
/// stored in that project's sessions DB, and replayed into message 0 — inside
/// the cached prefix. Following a `cd` would switch projects, find no decision
/// there, and rewrite message 0. The opening statement sits in message 0 and
/// never changes, so neither does the project.
pub fn extract_opening_prompt(body: &Value) -> String {
    lead_with_cwd(stated_cwds(body).into_iter().next(), body)
}

fn lead_with_cwd(cwd: Option<String>, body: &Value) -> String {
    let system = extract_system_prompt(body);
    match cwd {
        Some(cwd) => format!("{} {cwd}\n{system}", CWD_PREFIXES[0]),
        None => system,
    }
}

/// Every working directory the client stated in `messages`, oldest first.
///
/// Only a line under an `# Environment` heading counts. Tool results,
/// assistant turns, and the recall and memory blocks the proxy adds itself all
/// quote the line as often as not, so they are skipped.
fn stated_cwds(body: &Value) -> Vec<String> {
    let mut stated = Vec::new();
    for msg in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if !matches!(
            msg.get("role").and_then(Value::as_str),
            Some("user" | "system")
        ) {
            continue;
        }
        let texts: Vec<&str> = match msg.get("content") {
            Some(Value::String(s)) => vec![s.as_str()],
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect(),
            _ => Vec::new(),
        };
        let own = |t: &&str| {
            t.contains(CWD_PREFIXES[0])
                && !t
                    .trim_start()
                    .starts_with(headroom_core::ctx::INJECT_SENTINEL)
                && !t.contains("<memory_context>")
        };
        for text in texts.into_iter().filter(own) {
            let mut in_environment = false;
            for line in text.lines() {
                let line = line.trim_start();
                if line.starts_with("# Environment") {
                    in_environment = true;
                    continue;
                }
                let Some(rest) = line
                    .trim_start_matches("- ")
                    .strip_prefix(CWD_PREFIXES[0])
                    .filter(|_| in_environment)
                else {
                    continue;
                };
                let cwd = rest.split(" (was ").next().unwrap_or(rest).trim();
                if !cwd.is_empty() {
                    stated.push(cwd.to_string());
                }
            }
        }
    }
    stated
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- ProjectResolver ---

    /// One repository, one pool. A subdirectory, a linked worktree and the
    /// root itself must all resolve to the same project, or a memory saved
    /// from a worktree is invisible from the checkout that made it.
    #[test]
    fn worktrees_and_subdirectories_share_the_repository_key() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("shopkit");
        std::fs::create_dir_all(repo.join(".git/worktrees/access-gates")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(repo.join("apps/api")).unwrap();

        let worktree = tmp.path().join("wt-access-gates");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}/.git/worktrees/access-gates\n", repo.display()),
        )
        .unwrap();

        let key = |path: &std::path::Path| {
            ProjectResolver::resolve(&RequestContext {
                headers: HashMap::new(),
                system_prompt: format!("Primary working directory: {}\n", path.display()),
                base_user_id: "default".to_string(),
                project_root_override: None,
            })
            .expect("cwd resolves")
            .0
        };

        let root = key(&repo);
        assert_eq!(key(&repo.join("apps/api")), root, "subdirectory split off");
        assert_eq!(key(&worktree), root, "worktree split off");
        assert!(root.starts_with("shopkit-"), "unexpected key {root}");
    }

    /// `--memory-project-root` is a last resort, not an override. A request
    /// that names its own cwd in the system prompt must keep its own project,
    /// or one operator setting would pull every repo's turns into one
    /// workspace — exactly the bleed this module exists to stop.
    #[test]
    fn system_prompt_cwd_outranks_the_cli_override() {
        let tmp = tempfile::tempdir().unwrap();
        let own = tmp.path().join("its-own-repo");
        let fallback = tmp.path().join("operator-fallback");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();
        let resolve = |system_prompt: &str| {
            ProjectResolver::resolve(&RequestContext {
                headers: HashMap::new(),
                system_prompt: system_prompt.to_string(),
                base_user_id: "default".to_string(),
                project_root_override: Some(fallback.display().to_string()),
            })
            .expect("resolves")
            .0
        };
        let with_cwd = resolve(&format!("Primary working directory: {}", own.display()));
        let without_cwd = resolve("You are a helpful assistant.");
        assert_ne!(
            with_cwd, without_cwd,
            "a request naming its own cwd must not collapse into the override"
        );
        assert_eq!(
            without_cwd,
            ProjectResolver::resolve(&RequestContext {
                headers: HashMap::new(),
                system_prompt: String::new(),
                base_user_id: "default".to_string(),
                project_root_override: Some(fallback.display().to_string()),
            })
            .expect("resolves")
            .0,
            "a request with no cwd anywhere must land on the override"
        );
    }

    /// A directory with no repository above it is still its own project —
    /// separation is the point, and collapsing everything into one pool would
    /// be worse than a spare partition.
    #[test]
    fn a_plain_directory_keeps_its_own_key() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("alpha");
        let b = tmp.path().join("beta");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let key = |path: &std::path::Path| {
            ProjectResolver::resolve(&RequestContext {
                headers: HashMap::new(),
                system_prompt: String::new(),
                base_user_id: "default".to_string(),
                project_root_override: Some(path.display().to_string()),
            })
            .expect("override resolves")
            .0
        };
        assert_ne!(key(&a), key(&b));
    }

    /// A repository moved to another folder, or cloned again over ssh instead
    /// of https, is the same project. One with no `origin` has nothing else to
    /// go on and stays keyed by its path.
    #[test]
    fn the_origin_remote_keys_a_repository_wherever_it_sits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = |dir: &str, origin: Option<&str>| {
            let path = tmp.path().join(dir);
            std::fs::create_dir_all(path.join(".git")).unwrap();
            std::fs::write(path.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
            let mut config = "[core]\n\tbare = false\n".to_string();
            if let Some(url) = origin {
                config.push_str(&format!(
                    "[remote \"upstream\"]\n\turl = https://github.com/someone/else.git\n\
                     [remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"
                ));
            }
            std::fs::write(path.join(".git/config"), config).unwrap();
            path
        };
        let key = |path: &std::path::Path| {
            ProjectResolver::resolve(&RequestContext {
                headers: HashMap::new(),
                system_prompt: format!("Primary working directory: {}\n", path.display()),
                base_user_id: "default".to_string(),
                project_root_override: None,
            })
            .expect("cwd resolves")
        };

        let before = key(&repo(
            "ext/capex-analysis",
            Some("https://github.com/tzhang02-code/capex-analysis.git"),
        ));
        let moved = key(&repo(
            "workspace/capex",
            Some("git@github.com:tzhang02-code/capex-analysis.git"),
        ));
        assert_eq!(moved, before, "moving or re-cloning split the project");
        assert_eq!(before.1, "capex-analysis");
        assert!(
            before.0.starts_with("capex-analysis-"),
            "unexpected key {}",
            before.0
        );

        let other = key(&repo(
            "ext/letro",
            Some("https://github.com/HugoDulce/letro.git"),
        ));
        assert_ne!(other.0, before.0, "different remotes share a project");

        // Without an origin, two copies are two projects.
        assert_ne!(key(&repo("a/local", None)).0, key(&repo("b/local", None)).0);
        assert_ne!(
            key(&repo("a/rel", Some("../rel.git"))).0,
            key(&repo("b/rel", Some("../rel.git"))).0,
            "a relative remote is not an identity"
        );
    }

    /// The partition the backend actually uses.
    #[test]
    fn scoped_user_id_carries_the_project() {
        let ctx = |project: &str| RequestContext {
            headers: HashMap::from([("x-headroom-project-id".to_string(), project.to_string())]),
            system_prompt: String::new(),
            base_user_id: "default".to_string(),
            project_root_override: None,
        };
        assert_eq!(
            scoped_user_id("default", &ctx("shopkit")),
            "default::shopkit"
        );
        assert_ne!(
            scoped_user_id("default", &ctx("shopkit")),
            scoped_user_id("default", &ctx("headroom"))
        );
        // Nothing resolved: one shared pool rather than a lost write.
        assert_eq!(
            scoped_user_id(
                "default",
                &RequestContext {
                    headers: HashMap::new(),
                    system_prompt: String::new(),
                    base_user_id: "default".to_string(),
                    project_root_override: None,
                }
            ),
            "default"
        );
    }

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
        router.touch(Path::new("/a"));
        router.touch(Path::new("/b"));
        router.touch(Path::new("/c"));
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

    /// The ctx stores stay with the repository a conversation opened in: a
    /// subdirectory, a worktree and a moved clone share them, a `cd` does not
    /// move them, the operator fallback only catches requests that name no
    /// directory, and lines the proxy or the tools quote are not statements.
    #[test]
    fn the_ctx_project_is_the_repository_the_conversation_opened_in() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = |dir: &str| {
            let path = tmp.path().join(dir);
            std::fs::create_dir_all(path.join(".git/worktrees/wt")).unwrap();
            std::fs::create_dir_all(path.join("apps/api")).unwrap();
            std::fs::write(path.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
            std::fs::write(
                path.join(".git/config"),
                "[remote \"origin\"]\n\turl = git@github.com:acme/shop.git\n",
            )
            .unwrap();
            path
        };
        let (shop, moved) = (repo("ext/shop"), repo("workspace/shop"));
        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}/.git/worktrees/wt\n", shop.display()),
        )
        .unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let fallback = tmp.path().join("fallback");
        std::fs::create_dir_all(&fallback).unwrap();

        let environment = |dir: &std::path::Path| {
            format!(
                "<system-reminder>\n# Environment\nYou have been invoked in the following \
                 environment: \n - Primary working directory: {}\n</system-reminder>",
                dir.display()
            )
        };
        let opened_in = |dir: &std::path::Path, later: Vec<Value>| {
            let mut messages = vec![json!({"role": "user", "content": [
                {"type": "text", "text": format!(
                    "{}\n<session_recall>\n# Environment\n - Primary working directory: {}\n</session_recall>",
                    headroom_core::ctx::INJECT_SENTINEL, elsewhere.display()
                )},
                {"type": "text", "text": environment(dir)},
                {"type": "text", "text": "build the cart"},
            ]})];
            messages.extend(later);
            json!({"system": [{"type": "text", "text": "You are Claude Code."}], "messages": messages})
        };
        let project = |body: &Value| {
            ProjectResolver::resolve_project_dir(&RequestContext {
                headers: HashMap::new(),
                system_prompt: extract_opening_prompt(body),
                base_user_id: String::new(),
                project_root_override: Some(fallback.display().to_string()),
            })
            .expect("resolves")
        };

        let root = project(&opened_in(&shop, vec![]));
        assert_eq!(root, "github.com/acme/shop");
        assert_eq!(project(&opened_in(&shop.join("apps/api"), vec![])), root);
        assert_eq!(project(&opened_in(&worktree, vec![])), root);
        assert_eq!(project(&opened_in(&moved, vec![])), root);

        let cd = json!({"role": "system", "content": format!(
            "# Environment update\n - Primary working directory: {} (was {})",
            elsewhere.display(), shop.display()
        )});
        let quoted = json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t", "content": environment(&elsewhere)},
            {"type": "text", "text": format!("<memory_context>\n{}\n</memory_context>", environment(&elsewhere))},
            {"type": "text", "text": format!("Primary working directory: {}", elsewhere.display())},
        ]});
        let later = opened_in(&shop, vec![quoted, cd]);
        assert_eq!(
            project(&later),
            root,
            "the ctx project moved mid-conversation"
        );
        // Memory, injected at the tail, does follow the cd.
        assert!(extract_project_prompt(&later).starts_with(&format!(
            "Primary working directory: {}\n",
            elsewhere.display()
        )));

        let silent = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(project(&silent), fallback.display().to_string());
    }

    /// Shaped like a current Claude Code request: nothing in `system`, the
    /// directory in the opening user message, a `cd` announced later, and the
    /// line quoted in a tool result and by the assistant along the way.
    #[test]
    fn the_project_follows_the_directory_claude_code_states_last() {
        let tmp = tempfile::tempdir().unwrap();
        let (opened, moved) = (tmp.path().join("workspace"), tmp.path().join("capex"));
        std::fs::create_dir_all(&opened).unwrap();
        std::fs::create_dir_all(&moved).unwrap();
        let (opened, moved) = (opened.display().to_string(), moved.display().to_string());
        let quoted = tmp.path().join("quoted").display().to_string();
        let resolve = |body: &Value| {
            ProjectResolver::resolve(&RequestContext {
                headers: HashMap::new(),
                system_prompt: extract_project_prompt(body),
                base_user_id: "default".to_string(),
                project_root_override: Some("/operator/fallback".to_string()),
            })
            .expect("resolves")
            .1
        };

        let mut body = json!({
            "system": [{"type": "text", "text": "You are Claude Code."}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": format!(
                    "# Environment\n - Primary working directory: {opened}\n - Is a git repository: true"
                )}]},
            ]
        });
        assert_eq!(resolve(&body), "workspace", "opening directory missed");

        let messages = body["messages"].as_array_mut().unwrap();
        messages.push(json!({"role": "assistant", "content": [
            {"type": "text", "text": format!("Primary working directory: {quoted}")}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": format!("Primary working directory: {quoted}")}
        ]}));
        assert_eq!(
            resolve(&body),
            "workspace",
            "a quoted line moved the project"
        );

        body["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "system", "content": format!(
                "# Environment update\n - Primary working directory: {moved} (was {opened})"
            )}));
        assert_eq!(resolve(&body), "capex", "a later cd was not followed");

        // No directory anywhere: the operator fallback, as before.
        assert_eq!(
            resolve(&json!({"messages": [{"role": "user", "content": "hi"}]})),
            "fallback"
        );
    }

    #[test]
    fn extract_system_empty_body() {
        assert!(extract_system_prompt(&json!({})).is_empty());
    }
}
