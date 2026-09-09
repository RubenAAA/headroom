//! Reversible redaction for routed-model paths.
//!
//! # Why this exists
//!
//! Some routed upstreams buy their price with your data: the Contributor Free
//! tier trades discounted tokens for permission to train on prompts and
//! completions. Nothing in a header opts out of that — the consent is the
//! model choice itself. So when redaction is on, the proxy rewrites sensitive
//! spans before translation, and rewrites them back on everything handed to
//! the client.
//!
//! # What the model sees
//!
//! Your home directory arrives as `__HR_HOME__` with the rest of the path in
//! the clear (`__HR_HOME__/headroom/src/main.py`): the username and machine
//! layout stay home, while the project structure the model needs to reason
//! survives. Anything else sensitive — other users' homes, secrets, emails —
//! goes out as a fully opaque token (`__HR_PATH_7__`). Secrets and emails
//! are never partially shown.
//!
//! # The round trip, and why one-way redaction would break tools
//!
//! A placeholder that only goes out is a lie the client has to live with: the
//! model calls `Read` on a token, Claude Code has never heard of it, and the
//! turn dies on a file that does not exist. So the map is kept per-session
//! and the restore pass runs at the proxy edge — buffered bodies, SSE
//! streams, `tool_use` inputs and text alike — before the client sees a byte.
//! Tool results coming back up are re-redacted through the same map, so a
//! path keeps one placeholder for the whole session and the provider's cached
//! prefix stays stable across turns.
//!
//! # What is (and is not) covered
//!
//! - Covered: `system` and `messages` strings on the routed translate path,
//!   including `tool_result` content and echoed `tool_use` input.
//! - Not covered: the `tools` array (schemas are not echoed back, and examples
//!   in them guide the model), local artifacts (logs, the sessions DB — same
//!   trust as today), and the spinner sidecar's routed attempt (skipped while
//!   redaction is on; the direct path is unaffected).
//! - Placeholders are ASCII (`__HR_PATH_7__`), so they survive JSON, SSE and
//!   provider translation byte-equal, and stream restores never need to parse.
//!
//! # Streaming
//!
//! A placeholder may straddle two SSE chunks. The restore stream therefore
//! holds back a tail of `longest-placeholder - 1` bytes and only emits what
//! no future chunk can still complete. Placeholders are self-delimiting, so
//! the scan is a single pass with no backtracking.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::Stream;
use lru::LruCache;
use serde_json::Value;

/// Sessions remembered. Oldest goes when full; its placeholders then stop
/// restoring, which is loud (a miss counter) rather than silent.
const STORE_CAPACITY: usize = 128;
/// Placeholders per session. Same deal: eviction orphans ancient history, and
/// the miss counter says so.
const SESSION_CAPACITY: usize = 512;
/// Hard ceiling on the streaming hold-back, whatever the map holds.
const MAX_OVERLAP: usize = 64;

const PREFIX: &str = "__HR_";
const PATH_KIND: &str = "PATH";
const SECRET_KIND: &str = "SECRET";
const EMAIL_KIND: &str = "EMAIL";
/// The one non-counted placeholder: your home directory. The remainder of a
/// home-rooted path stays in the clear behind it, so the model keeps the
/// project structure it needs to reason with.
const HOME_TOKEN: &str = "__HR_HOME__";

/// What the path scanner found.
enum PathHit {
    /// Redact the whole span into an opaque token.
    Opaque(usize),
    /// Home-rooted: replace the first `prefix` bytes with the home token and
    /// leave the remainder in the clear.
    Home { total: usize, prefix: usize },
}

/// Object keys whose string values are never redacted: ids must keep matching
/// across the turn, signatures must keep verifying, and `data` is base64 that
/// the entropy scanner would eat alive.
const SKIP_KEYS: &[&str] = &[
    "id",
    "tool_use_id",
    "tool_call_id",
    "call_id",
    "signature",
    "data",
];
/// Absolute prefixes that stay in the clear: system paths the model needs to
/// understand, and that name nothing of yours.
const CLEAR_PREFIXES: &[&str] = &[
    "/usr/", "/bin/", "/sbin/", "/lib/", "/lib64/", "/etc/", "/dev/", "/proc/", "/sys/", "/var/",
    "/tmp/", "/opt/",
];

/// One session's two-way map. `forward` restores, `reverse` keeps a value on
/// one placeholder for the whole session, `order` bounds memory.
///
/// Placeholders are deterministic in `(session, kind, value)`, not counted:
/// the same value redacts to the same token across turns, restarts and
/// evictions, so the provider's cached prefix survives all three. The session
/// key doubles as the hash salt — it never leaves the box — so identical
/// values in different sessions mint different, mutually opaque tokens.
struct SessionMap {
    forward: HashMap<String, String>,
    reverse: HashMap<String, String>,
    order: VecDeque<String>,
}

impl SessionMap {
    fn new() -> Self {
        Self {
            forward: HashMap::new(),
            reverse: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn placeholder(&mut self, kind: &'static str, original: &str, session: &str) -> String {
        if let Some(existing) = self.reverse.get(original) {
            return existing.clone();
        }
        // 64-bit truncation: a collision would restore the wrong secret, so
        // on the paranoia that two values in one session ever collide, salt
        // in an attempt counter rather than aliasing.
        let mut attempt = 0u32;
        let token = loop {
            let candidate = token_for(session, kind, original, attempt);
            match self.forward.get(&candidate) {
                None => break candidate,
                Some(owner) if owner == original => break candidate,
                Some(_) => attempt += 1,
            }
        };
        if self.forward.len() >= SESSION_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                if let Some(orig) = self.forward.remove(&oldest) {
                    self.reverse.remove(&orig);
                }
            }
        }
        self.forward.insert(token.clone(), original.to_string());
        self.reverse.insert(original.to_string(), token.clone());
        self.order.push_back(token.clone());
        token
    }

    /// The home-directory token. Constant, uncounted, and outside the eviction
    /// order: every home-rooted path in the session shares its prefix, so
    /// evicting it would orphan them all at once.
    fn home_token(&mut self, home: &str) -> String {
        if !self.forward.contains_key(HOME_TOKEN) {
            self.forward
                .insert(HOME_TOKEN.to_string(), home.to_string());
            self.reverse
                .insert(home.to_string(), HOME_TOKEN.to_string());
        }
        HOME_TOKEN.to_string()
    }
}

/// Per-session redaction memory, keyed on `session_key` like the roster and
/// order stores. The maps hold your secrets in process memory only — never
/// serialized, never logged.
#[derive(Clone)]
pub struct RedactStore {
    inner: Arc<Mutex<LruCache<String, SessionMap>>>,
}
impl std::fmt::Debug for RedactStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedactStore").finish_non_exhaustive()
    }
}

impl Default for RedactStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RedactStore {
    pub fn new() -> Self {
        let capacity =
            std::num::NonZeroUsize::new(STORE_CAPACITY).unwrap_or(std::num::NonZeroUsize::MIN);
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(capacity))),
        }
    }

    fn with_session(&self, session_key: &str, f: impl FnOnce(&mut SessionMap)) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let map = guard.get_or_insert_mut(session_key.to_string(), SessionMap::new);
        f(map);
    }
}

/// Handle to redact or restore under one session, threaded into continuation
/// resolvers so content fetched mid-turn (memory answers, cold-tier blocks)
/// is redacted before it joins an upstream-bound request.
#[derive(Clone, Debug)]
pub struct RedactRef {
    pub store: RedactStore,
    pub session_key: String,
}

/// Redact one string. Used for content fetched mid-turn.
pub fn redact_string(r: &RedactRef, text: &str) -> String {
    BodyRedactor::new(&r.store, &r.session_key)
        .redact_text(text)
        .0
}

/// Redact every eligible string in a provider-shaped value. Used for tool
/// results assembled mid-turn.
pub fn redact_value(r: &RedactRef, v: &mut Value) {
    let mut redactor = BodyRedactor::new(&r.store, &r.session_key);
    redactor.walk_value(v);
}

/// What one outbound pass did. Counts only — values never leave the map.
pub struct RedactReport {
    pub spans_redacted: usize,
    pub placeholders_live: usize,
}

/// Redact `system` and `messages` strings in place. Returns what was done.
pub fn redact_body(store: &RedactStore, session_key: &str, body: &mut Value) -> RedactReport {
    let mut redactor = BodyRedactor::new(store, session_key);
    redactor.walk_body(body);
    let placeholders_live = store_size(store, session_key);
    RedactReport {
        spans_redacted: redactor.spans,
        placeholders_live,
    }
}

/// Undo [`redact_body`]: restore placeholders to their originals. Used when a
/// routed turn falls back to the client's own model, so the fallback forwards
/// what the client sent rather than the redacted body.
pub fn unredact_body(store: &RedactStore, session_key: &str, body: &mut Value) {
    let forward = forward_snapshot(store, session_key);
    if forward.is_empty() {
        return;
    }
    walk_strings(body, &mut |s| {
        let (out, _) = restore_str(&forward, s);
        *s = out;
    });
}

struct BodyRedactor<'a> {
    store: &'a RedactStore,
    session_key: String,
    home: Option<String>,
    spans: usize,
}

impl<'a> BodyRedactor<'a> {
    fn new(store: &'a RedactStore, session_key: &str) -> Self {
        Self {
            store,
            session_key: session_key.to_string(),
            home: std::env::var("HOME").ok().filter(|h| h.starts_with('/')),
            spans: 0,
        }
    }

    fn walk_body(&mut self, body: &mut Value) {
        if let Some(system) = body.get_mut("system") {
            self.walk_value(system);
        }
        if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for msg in messages {
                self.walk_value(msg);
            }
        }
    }

    fn walk_value(&mut self, v: &mut Value) {
        match v {
            Value::String(s) => {
                let (out, n) = self.redact_text(s);
                *s = out;
                self.spans += n;
            }
            Value::Array(items) => {
                for item in items {
                    self.walk_value(item);
                }
            }
            Value::Object(map) => {
                for (k, val) in map.iter_mut() {
                    if SKIP_KEYS.contains(&k.as_str()) {
                        continue;
                    }
                    self.walk_value(val);
                }
            }
            _ => {}
        }
    }

    fn redact_text(&mut self, text: &str) -> (String, usize) {
        // Slicing invariant: every index below is a char boundary. `i`
        // starts at 0 and advances by whole chars or whole matches, and every
        // matcher only consumes or splits on ASCII bytes — a run can end at,
        // but never inside, a multibyte char.
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        let mut count = 0;
        while i < bytes.len() {
            if let Some((off, len, kind)) = self.match_secret(bytes, i) {
                // A match can start past `i` (pgpass / URL passwords keep
                // their prefix in the clear); emit the gap literally.
                out.push_str(&text[i..i + off]);
                let original = &text[i + off..i + off + len];
                let token = self.mint(kind, original);
                out.push_str(&token);
                count += 1;
                i += off + len;
            } else if let Some(len) = match_email(bytes, i) {
                let original = &text[i..i + len];
                let token = self.mint(EMAIL_KIND, original);
                out.push_str(&token);
                count += 1;
                i += len;
            } else if let Some(hit) = self.match_path(bytes, i) {
                match hit {
                    PathHit::Opaque(len) => {
                        let original = &text[i..i + len];
                        let token = self.mint(PATH_KIND, original);
                        out.push_str(&token);
                        count += 1;
                        i += len;
                    }
                    PathHit::Home { total, prefix } => {
                        let home = self.home.clone().unwrap_or_default();
                        let mut token = String::new();
                        self.store.with_session(&self.session_key, |map| {
                            token = map.home_token(&home);
                        });
                        out.push_str(&token);
                        out.push_str(&text[i + prefix..i + total]);
                        count += 1;
                        i += total;
                    }
                }
            } else {
                // Advance by one char, not one byte.
                let ch_len = text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                out.push_str(&text[i..i + ch_len]);
                i += ch_len;
            }
        }
        (out, count)
    }

    fn mint(&mut self, kind: &'static str, original: &str) -> String {
        let mut token = String::new();
        let session = self.session_key.clone();
        self.store.with_session(&session, |map| {
            token = map.placeholder(kind, original, &session);
        });
        token
    }

    /// Secrets first: a path run must never swallow a key. Returns the match
    /// as `(offset, len)` from `i`: most matches start at `i`, but pgpass
    /// lines and database URLs keep their prefix in the clear and only the
    /// password goes.
    fn match_secret(&self, bytes: &[u8], i: usize) -> Option<(usize, usize, &'static str)> {
        let rest = &bytes[i..];
        // `-----BEGIN ...-----` through the END line: private keys and certs.
        if rest.starts_with(b"-----BEGIN ") {
            if let Some(end) = find_subslice(rest, b"-----END ") {
                let tail = &rest[end..];
                if let Some(nl) = tail.iter().position(|&b| b == b'\n') {
                    return Some((0, end + nl + 1, SECRET_KIND));
                }
                return Some((0, rest.len(), SECRET_KIND));
            }
        }
        // Known prefixes.
        for prefix in [
            b"sk-".as_slice(),
            b"ghp_".as_slice(),
            b"gho_".as_slice(),
            b"ghu_".as_slice(),
            b"ghs_".as_slice(),
            b"github_pat_".as_slice(),
            b"glpat-".as_slice(),
            b"xoxb-".as_slice(),
            b"xoxp-".as_slice(),
            b"xoxa-".as_slice(),
            b"xoxs-".as_slice(),
            b"xoxo-".as_slice(),
            b"xoxr-".as_slice(),
            b"hf_".as_slice(),
            b"dop_v1_".as_slice(),
            b"AKIA".as_slice(),
            b"ASIA".as_slice(),
            b"AIza".as_slice(),
        ] {
            if rest.starts_with(prefix) {
                let mut len = prefix.len();
                while len < rest.len() && is_token_char(rest[len]) {
                    len += 1;
                }
                if len - prefix.len() >= 8 {
                    return Some((0, len, SECRET_KIND));
                }
            }
        }
        // `name[:=] value`: the value goes, the name stays. Identifier chars
        // between the name and the separator cover `secret_key`, `api-key`,
        // `apiKey`, `AWS_SECRET_ACCESS_KEY`. `Bearer <token>` has no
        // separator. A rare prose false positive (`secretary = ...`) restores
        // exactly, so it costs confusion, never breakage.
        for name in [
            b"Bearer ".as_slice(),
            b"api_key".as_slice(),
            b"apikey".as_slice(),
            b"password".as_slice(),
            b"passwd".as_slice(),
            b"passphrase".as_slice(),
            b"pgpassword".as_slice(),
            b"secret".as_slice(),
            b"token".as_slice(),
        ] {
            if starts_word_insensitive(rest, name) {
                let mut j = name.len();
                if !name.ends_with(b" ") {
                    while j < rest.len() && is_ident_char(rest[j]) {
                        j += 1;
                    }
                    while j < rest.len() && (rest[j] == b' ' || rest[j] == b'\t') {
                        j += 1;
                    }
                    if j < rest.len() && (rest[j] == b':' || rest[j] == b'=') {
                        j += 1;
                    } else {
                        continue;
                    }
                    while j < rest.len() && (rest[j] == b' ' || rest[j] == b'\t') {
                        j += 1;
                    }
                    if j < rest.len() && (rest[j] == b'\'' || rest[j] == b'"') {
                        j += 1;
                    }
                }
                let start = j;
                while j < rest.len() && is_token_char(rest[j]) {
                    j += 1;
                }
                if j - start >= 8 {
                    return Some((start, j - start, SECRET_KIND));
                }
            }
        }
        // pgpass line `host:port:db:user:password` at a line start: only the
        // password goes, the endpoint stays for reasoning.
        if i == 0 || bytes[i - 1] == b'\n' {
            if let Some((off, len)) = match_pgpass_password(rest) {
                return Some((off, len, SECRET_KIND));
            }
        }
        // `postgres[ql]://user:password@host…`: only the password goes.
        for scheme in [b"postgres://".as_slice(), b"postgresql://".as_slice()] {
            if rest.starts_with(scheme) {
                if let Some((off, len)) = match_url_password(&rest[scheme.len()..]) {
                    return Some((scheme.len() + off, len, SECRET_KIND));
                }
            }
        }
        // AWS account id inside an ARN `:123456789012:`: the digits go.
        if rest.first() == Some(&b':') && rest.len() > 14 {
            let digits = &rest[1..13];
            if digits.iter().all(|b| b.is_ascii_digit()) && rest.get(13) == Some(&b':') {
                return Some((1, 12, SECRET_KIND));
            }
        }
        // JWT: three base64url segments.
        if rest.starts_with(b"eyJ") {
            let mut parts = 0;
            let mut j = 0;
            while j < rest.len() && (is_b64url(rest[j]) || rest[j] == b'.') {
                if rest[j] == b'.' {
                    parts += 1;
                }
                j += 1;
                if parts == 2 {
                    while j < rest.len() && is_b64url(rest[j]) {
                        j += 1;
                    }
                    break;
                }
            }
            if parts == 2 && j >= 32 {
                return Some((0, j, SECRET_KIND));
            }
        }
        // High-entropy run: long, mixed classes, not a UUID, not a filename
        // (a trailing `.ext` means file, and filenames are paths' business).
        let mut j = i;
        while j < bytes.len() && is_token_char(bytes[j]) {
            j += 1;
        }
        let len = j - i;
        if len >= 28
            && looks_secret(&bytes[i..j])
            && !looks_uuid(&bytes[i..j])
            && !has_extension(&bytes[i..j])
        {
            return Some((0, len, SECRET_KIND));
        }
        None
    }

    /// Home-rooted paths, `~/…`, absolute paths outside the clear prefixes,
    /// and relative multi-segment paths (`a/b/c`, `src/main.py`). Never URLs.
    /// Your own home dir is a [`PathHit::Home`] (prefix hidden, rest clear);
    /// anything else sensitive is [`PathHit::Opaque`].
    fn match_path(&self, bytes: &[u8], i: usize) -> Option<PathHit> {
        let rest = &bytes[i..];
        let home_known = self.home.is_some();
        // `$HOME/...` and `${HOME}/...`.
        for prefix in [b"$HOME".as_slice(), b"${HOME}".as_slice()] {
            if rest.starts_with(prefix) {
                let mut len = prefix.len();
                while len < rest.len() && is_path_char(rest[len]) {
                    len += 1;
                }
                if home_known {
                    return Some(PathHit::Home {
                        total: len,
                        prefix: prefix.len(),
                    });
                }
                return Some(PathHit::Opaque(len));
            }
        }
        // `~/...`.
        if rest.starts_with(b"~/") {
            let mut len = 2;
            while len < rest.len() && is_path_char(rest[len]) {
                len += 1;
            }
            if home_known {
                return Some(PathHit::Home {
                    total: len,
                    prefix: 1,
                });
            }
            return Some(PathHit::Opaque(len));
        }
        // Explicit home dir, `/home/<user>/…`, `/root/…`, `/Users/<name>/…`.
        if let Some(home) = self.home.as_deref() {
            if rest.starts_with(home.as_bytes())
                && rest.len() > home.len()
                && is_path_char(rest[home.len()])
            {
                let mut len = home.len();
                while len < rest.len() && is_path_char(rest[len]) {
                    len += 1;
                }
                return Some(PathHit::Home {
                    total: len,
                    prefix: home.len(),
                });
            }
        }
        for prefix in [
            b"/home/".as_slice(),
            b"/root/".as_slice(),
            b"/Users/".as_slice(),
        ] {
            if rest.starts_with(prefix) {
                let mut len = prefix.len();
                while len < rest.len() && is_path_char(rest[len]) {
                    len += 1;
                }
                // Need something past the prefix itself.
                if len > prefix.len() + 1 {
                    return Some(PathHit::Opaque(len));
                }
            }
        }
        // Absolute path: starts at `/`, runs on path chars, holds a second
        // segment or a filename. Skips URLs (`://` inside or directly behind),
        // mid-word slashes (the relative branch owns those), and the clear
        // system prefixes.
        if rest[0] == b'/' {
            if i > 0 && is_boundary_char(bytes[i - 1]) {
                return None;
            } // Second slash of `//`: the first slash already declined the URL.
            if i > 0 && bytes[i - 1] == b'/' {
                return None;
            }
            // First slash of `://`: the scheme is directly behind.
            if i > 0 && bytes[i - 1] == b':' && rest.get(1) == Some(&b'/') {
                return None;
            }
            let mut len = 1;
            while len < rest.len() && is_path_char(rest[len]) {
                len += 1;
            }
            let run = &rest[..len];
            if run.contains(&b':') && run.windows(3).any(|w| w == b"://") {
                return None;
            }
            if len < 3 || !run[1..].contains(&b'/') && !has_extension(run) {
                // Single short segment (`/x`) or bare root — not a path worth
                // hiding, and usually not one at all.
                if !run[1..].contains(&b'/') {
                    return None;
                }
            }
            for clear in CLEAR_PREFIXES {
                if rest.starts_with(clear.as_bytes()) {
                    return None;
                }
            }
            return Some(PathHit::Opaque(len));
        }
        // Relative: needs two slashes (`a/b/c`) or one slash plus an
        // extension (`src/main.py`). Bare `a/b` is left alone — too often
        // division, a flag, or prose. A preceding slash means the absolute
        // branch owns this run (and already passed on it).
        if is_path_start(rest[0]) {
            if i > 0 && bytes[i - 1] == b'/' {
                return None;
            }
            let mut len = 0;
            while len < rest.len() && is_path_char(rest[len]) {
                len += 1;
            }
            let run = &rest[..len];
            // A `:` inside makes this a URI, not a path — the absolute
            // branch already declined it.
            if run.windows(3).any(|w| w == b"://") {
                return None;
            }
            if run.contains(&b'/') {
                let slashes = run.iter().filter(|&&b| b == b'/').count();
                if slashes >= 2 || (slashes == 1 && has_extension(run)) {
                    // Must start at a token boundary, or `import a/b` eats
                    // its own tail. A preceding joiner means mid-token.
                    if i > 0 && is_boundary_char(bytes[i - 1]) {
                        return None;
                    }
                    return Some(PathHit::Opaque(len));
                }
            }
        }
        None
    }
}

/// Mint a placeholder deterministically: SHA-256 over the session, kind and
/// value, NUL-separated the way `conversation_discriminator` separates its
/// fields, truncated to 64 bits (16 hex chars). The session key is the salt
/// and never goes upstream, so the token is opaque without it.
fn token_for(session: &str, kind: &str, original: &str, attempt: u32) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(session.as_bytes());
    hasher.update([0u8]);
    hasher.update(kind.as_bytes());
    hasher.update([0u8]);
    hasher.update(original.as_bytes());
    if attempt > 0 {
        hasher.update([0u8]);
        hasher.update(attempt.to_le_bytes());
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{PREFIX}{kind}_{hex}__")
}

fn store_size(store: &RedactStore, session_key: &str) -> usize {
    let mut n = 0;
    store.with_session(session_key, |map| {
        n = map.forward.len();
    });
    n
}

fn forward_snapshot(store: &RedactStore, session_key: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    store.with_session(session_key, |map| {
        out = map.forward.clone();
    });
    out
}

fn walk_strings(body: &mut Value, f: &mut impl FnMut(&mut String)) {
    match body {
        Value::String(s) => f(s),
        Value::Array(items) => {
            for item in items {
                walk_strings(item, f);
            }
        }
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if SKIP_KEYS.contains(&k.as_str()) {
                    continue;
                }
                walk_strings(v, f);
            }
        }
        _ => {}
    }
}

/// Snapshot of one session's map for response paths: no locking mid-stream.
pub struct RestoreTable {
    forward: HashMap<String, String>,
    max_len: usize,
}

/// Copy the session's map out. `None` when empty — the caller skips the pass.
pub fn restore_table(store: &RedactStore, session_key: &str) -> Option<RestoreTable> {
    let forward = forward_snapshot(store, session_key);
    if forward.is_empty() {
        return None;
    }
    let max_len = forward.keys().map(|k| k.len()).max().unwrap_or(0);
    Some(RestoreTable { forward, max_len })
}

impl RestoreTable {
    /// Restore every placeholder in `bytes`. Returns the bytes and the count
    /// of `__HR_` shapes that matched nothing — those stay as-is.
    pub fn restore_bytes(&self, bytes: &[u8]) -> (Vec<u8>, usize) {
        let mut out = Vec::with_capacity(bytes.len());
        let mut misses = 0;
        let mut i = 0;
        while i < bytes.len() {
            if let Some((token, len)) = parse_token(bytes, i) {
                if let Some(original) = self.forward.get(token) {
                    out.extend_from_slice(original.as_bytes());
                } else {
                    misses += 1;
                    out.extend_from_slice(token.as_bytes());
                }
                i += len;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        (out, misses)
    }

    fn overlap(&self) -> usize {
        self.max_len.saturating_sub(1).min(MAX_OVERLAP)
    }
}

fn restore_str(forward: &HashMap<String, String>, s: &str) -> (String, usize) {
    let table = RestoreTable {
        max_len: 0,
        forward: forward.clone(),
    };
    let (bytes, misses) = table.restore_bytes(s.as_bytes());
    (String::from_utf8_lossy(&bytes).into_owned(), misses)
}

/// Strict placeholder shape: `__HR_<KIND>_<hex>__`, plus the uncounted
/// `__HR_HOME__`. The hex run accepts the 16-char hash suffix and, so a
/// deploy mid-conversation degrades instead of breaking, the legacy 4-digit
/// counter suffix: after a restart the old token has no map entry and stays
/// as-is (counted as a miss) rather than corrupting text. Anything else is
/// bytes — including a `__HR_` the model invented.
fn parse_token(bytes: &[u8], i: usize) -> Option<(&str, usize)> {
    let rest = bytes.get(i..)?;
    if !rest.starts_with(PREFIX.as_bytes()) {
        return None;
    }
    let mut j = PREFIX.len();
    let kind_start = j;
    while j < rest.len() && rest[j].is_ascii_uppercase() {
        j += 1;
    }
    if j == kind_start || j >= rest.len() {
        return None;
    }
    let kind = std::str::from_utf8(&rest[kind_start..j]).ok()?;
    if !matches!(kind, "PATH" | "SECRET" | "EMAIL" | "HOME") {
        return None;
    }
    if j + 1 < rest.len() && rest[j] == b'_' && rest[j + 1] == b'_' {
        // Uncounted form (`__HR_HOME__`).
        j += 2;
    } else if j < rest.len() && rest[j] == b'_' {
        // Counted form: `_` plus 1-32 hex chars plus `__`. The upper bound
        // keeps a `__HR_PATH_` prefix in prose from eating the line.
        j += 1;
        let hex_start = j;
        while j < rest.len() && rest[j].is_ascii_hexdigit() && j - hex_start < 32 {
            j += 1;
        }
        if j == hex_start || j + 1 >= rest.len() || rest[j] != b'_' || rest[j + 1] != b'_' {
            return None;
        }
        j += 2;
    } else {
        return None;
    }
    std::str::from_utf8(&bytes[i..i + j]).ok().map(|t| (t, j))
}

/// A placeholder-restoring SSE adapter. A token may straddle two chunks, so
/// the stream holds back an overlap of `longest-token − 1` bytes and only
/// emits what no future chunk can still complete. Built on `poll_fn` so the
/// wrapped stream needs no `Unpin`.
pub fn restore_stream<S, E>(
    inner: S,
    table: RestoreTable,
) -> impl Stream<Item = Result<Bytes, E>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    let overlap = table.overlap();
    let mut inner = Box::pin(inner);
    let mut pending: Vec<u8> = Vec::new();
    let mut done = false;
    futures_util::stream::poll_fn(move |cx| {
        // The inner stream must never be polled after it ended: downstream
        // of this adapter sits an `Unfold` (the CCR rewrite), which panics
        // on a post-`None` poll, killing the worker and truncating the
        // client's SSE tail mid-frame ("Connection lost mid-response" on
        // every redacted routed turn). The tail below hides the end from
        // the consumer for one poll, so the guard has to run before the
        // poll, not after it.
        if done {
            return std::task::Poll::Ready(None);
        }
        match inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                pending.extend_from_slice(&chunk);
                // Never emit a token prefix: a token starting at or before
                // the cut but extending past it must stay in pending whole.
                // The search runs a needle-length past the cut so a start
                // straddling it is still found, then the cut pulls back to
                // the last start found.
                let mut emit_until = pending.len().saturating_sub(overlap);
                let search_end = (emit_until + PREFIX.len()).min(pending.len());
                if let Some(p) = last_prefix_start(&pending[..search_end]) {
                    emit_until = emit_until.min(p);
                }
                let (out, misses) = table.restore_bytes(&pending[..emit_until]);
                if misses > 0 {
                    tracing::debug!(misses, "placeholders the map could not restore; left as-is");
                }
                pending.drain(..emit_until);
                std::task::Poll::Ready(Some(Ok(Bytes::from(out))))
            }
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => {
                // Mark ended before emitting: the tail hides the end from
                // the consumer for one poll, and the next poll must not
                // touch the terminated inner (see the guard above).
                done = true;
                if pending.is_empty() {
                    return std::task::Poll::Ready(None);
                }
                let tail = std::mem::take(&mut pending);
                let (out, misses) = table.restore_bytes(&tail);
                if misses > 0 {
                    tracing::debug!(misses, "placeholders the map could not restore; left as-is");
                }
                std::task::Poll::Ready(Some(Ok(Bytes::from(out))))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    })
}

// --- scanners (no regex; see volatile_detector for why) ---

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b'+' | b'=')
}

/// Identifier chars between a key name and its separator (`secret_key`,
/// `api-key`, `apiKey`).
fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

fn is_b64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

fn is_path_char(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'/' | b'_' | b'-' | b'.' | b'~' | b'@' | b'+' | b',' | b':' | b'=' | b'%'
        )
}

fn is_path_start(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'~' || b == b'.'
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// What counts as "mid-token" for a path boundary: word chars plus the
/// joiners that glue tokens together (dots in domains/versions, dashes,
/// `@`). Without these, `opencode.ai/zen/v1` matches at `.ai/…`.
fn is_boundary_char(b: u8) -> bool {
    is_word_char(b) || matches!(b, b'.' | b'-' | b'@')
}

fn has_extension(run: &[u8]) -> bool {
    if let Some(dot) = run.iter().rposition(|&b| b == b'.') {
        let ext = &run[dot + 1..];
        !ext.is_empty() && ext.len() <= 6 && ext.iter().all(|b| b.is_ascii_alphanumeric())
    } else {
        false
    }
}

fn starts_word_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Password field of a pgpass line `host:port:db:user:password`, as
/// `(offset, len)` from the line start. Only the password goes — the endpoint
/// stays for reasoning.
fn match_pgpass_password(line: &[u8]) -> Option<(usize, usize)> {
    let mut j = 0;
    // host
    let mut k = j;
    while k < line.len() && line[k] != b':' && !line[k].is_ascii_whitespace() {
        k += 1;
    }
    if k == j || k >= line.len() || line[k] != b':' {
        return None;
    }
    j = k + 1;
    // port: all digits
    k = j;
    while k < line.len() && line[k].is_ascii_digit() {
        k += 1;
    }
    if k == j || k - j > 5 || k >= line.len() || line[k] != b':' {
        return None;
    }
    // db, user: two more colon fields
    for _ in 0..2 {
        j = k + 1;
        k = j;
        while k < line.len() && line[k] != b':' && !line[k].is_ascii_whitespace() {
            k += 1;
        }
        if k == j || k >= line.len() || line[k] != b':' {
            return None;
        }
    }
    // password: runs to whitespace or end
    j = k + 1;
    k = j;
    while k < line.len() && !line[k].is_ascii_whitespace() {
        k += 1;
    }
    if k == j {
        return None;
    }
    Some((j, k - j))
}

/// Password of `user:password@host…` (past the scheme), as `(offset, len)`.
/// The password runs to the LAST `@` in the authority — passwords may contain
/// `@` themselves (`p@ss`). Only the password goes.
fn match_url_password(rest: &[u8]) -> Option<(usize, usize)> {
    let mut a = 0;
    while a < rest.len() && rest[a] != b'/' && !rest[a].is_ascii_whitespace() {
        a += 1;
    }
    let auth = &rest[..a];
    let at = auth.iter().rposition(|&b| b == b'@')?;
    let colon = auth[..at].iter().position(|&b| b == b':')?;
    if colon == 0 || at - colon < 2 {
        return None;
    }
    Some((colon + 1, at - colon - 1))
}

/// Last index in `buf` where a placeholder could start, so the stream cut
/// never severs one.
fn last_prefix_start(buf: &[u8]) -> Option<usize> {
    let needle = PREFIX.as_bytes();
    if buf.len() < needle.len() {
        return None;
    }
    (0..=buf.len() - needle.len())
        .rev()
        .find(|&i| &buf[i..i + needle.len()] == needle)
}

fn looks_secret(run: &[u8]) -> bool {
    let mut upper = false;
    let mut lower = false;
    let mut digit = false;
    let mut symbol = false;
    for &b in run {
        if b.is_ascii_uppercase() {
            upper = true;
        } else if b.is_ascii_lowercase() {
            lower = true;
        } else if b.is_ascii_digit() {
            digit = true;
        } else {
            symbol = true;
        }
    }
    [upper, lower, digit, symbol].iter().filter(|&&c| c).count() >= 3
}

fn looks_uuid(run: &[u8]) -> bool {
    run.len() == 36 && run[8] == b'-' && run[13] == b'-' && run[18] == b'-' && run[23] == b'-'
}

fn match_email(bytes: &[u8], i: usize) -> Option<usize> {
    // Forward-only: the main loop meets every email at its first character.
    let is_local =
        |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-');
    if i > 0 && is_local(bytes[i - 1]) {
        return None;
    }
    let mut j = i;
    while j < bytes.len() && is_local(bytes[j]) {
        j += 1;
    }
    if j == i || j >= bytes.len() || bytes[j] != b'@' {
        return None;
    }
    let local = &bytes[i..j];
    if local.is_empty() || local.len() > 64 || !local[0].is_ascii_alphanumeric() {
        return None;
    }
    j += 1;
    let domain_start = j;
    while j < bytes.len()
        && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-' || bytes[j] == b'.')
    {
        j += 1;
    }
    let domain = bytes.get(domain_start..j).unwrap_or(b"");
    if domain.contains(&b'.') {
        if domain.len() < 3 {
            return None;
        }
    } else {
        // Dotless `user@host` (`deploy@db-primary`, `root@localhost`):
        // worth hiding, but `A@B` matrix code is not an address.
        if local.len() < 2 || domain.len() < 2 {
            return None;
        }
    }
    Some(j - i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::json;

    fn store() -> RedactStore {
        RedactStore::new()
    }

    #[test]
    fn home_paths_round_trip() {
        let s = store();
        let mut body = json!({"messages": [
            {"role": "user", "content": "read /home/ruben/headroom/src/main.rs please"}
        ]});
        let report = redact_body(&s, "sess", &mut body);
        assert_eq!(report.spans_redacted, 1);
        let text = body["messages"][0]["content"].as_str().unwrap();
        // Prefix hidden, structure clear.
        assert_eq!(text, "read __HR_HOME__/headroom/src/main.rs please");
        unredact_body(&s, "sess", &mut body);
        assert_eq!(
            body["messages"][0]["content"].as_str().unwrap(),
            "read /home/ruben/headroom/src/main.rs please"
        );
    }

    #[test]
    fn other_users_homes_stay_opaque() {
        let s = store();
        let mut body = json!({"messages": [
            {"role": "user", "content": "check /home/alice/work/notes.md"}
        ]});
        redact_body(&s, "sess", &mut body);
        let text = body["messages"][0]["content"].as_str().unwrap();
        let token = text.strip_prefix("check ").expect("shape");
        assert_token_shape(token, "PATH", &text);
        unredact_body(&s, "sess", &mut body);
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("/home/alice/work/notes.md"));
    }

    #[test]
    fn invented_placeholder_kinds_do_not_restore() {
        let s = store();
        let mut body = json!({"messages": [{"role": "user", "content": "/home/ruben/a.py"}]});
        redact_body(&s, "sess", &mut body);
        let table = restore_table(&s, "sess").unwrap();
        let (out, misses) = table.restore_bytes(b"run __HR_FOO_0001__ and __HR_PATH_9999__");
        assert_eq!(misses, 1, "unknown kind is bytes, unknown number is a miss");
        assert_eq!(out, b"run __HR_FOO_0001__ and __HR_PATH_9999__");
    }

    #[test]
    fn same_path_keeps_one_placeholder_across_turns() {
        let s = store();
        let mut a = json!({"messages": [{"role": "user", "content": "/home/ruben/a.py"}]});
        let mut b = json!({"messages": [{"role": "user", "content": "fix /home/ruben/a.py"}]});
        redact_body(&s, "sess", &mut a);
        redact_body(&s, "sess", &mut b);
        assert_eq!(
            a["messages"][0]["content"].as_str().unwrap(),
            b["messages"][0]["content"]
                .as_str()
                .unwrap()
                .replace("fix ", "")
        );
    }

    #[test]
    fn tool_use_input_is_redacted_and_restores() {
        let s = store();
        let mut body = json!({"messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_01abc", "name": "Read",
                 "input": {"file_path": "/home/ruben/secret/keys.txt"}}
            ]}
        ]});
        redact_body(&s, "sess", &mut body);
        let input = &body["messages"][0]["content"][0]["input"];
        assert_eq!(
            input["file_path"].as_str().unwrap(),
            "__HR_HOME__/secret/keys.txt"
        );
        // The tool_use id must survive untouched.
        assert_eq!(body["messages"][0]["content"][0]["id"], "toolu_01abc");
        let table = restore_table(&s, "sess").unwrap();
        let (out, misses) = table.restore_bytes(br#"{"file_path": "__HR_HOME__/secret/keys.txt"}"#);
        assert_eq!(misses, 0);
        assert_eq!(
            out,
            br#"{"file_path": "/home/ruben/secret/keys.txt"}"#.as_slice()
        );
    }

    #[test]
    fn urls_and_system_paths_stay() {
        let s = store();
        let mut body = json!({"messages": [
            {"role": "user", "content": "see https://opencode.ai/zen/v1 and /usr/bin/env python3 and a/b"}
        ]});
        let report = redact_body(&s, "sess", &mut body);
        assert_eq!(report.spans_redacted, 0);
    }

    #[test]
    fn secrets_and_emails_go() {
        let s = store();
        let mut body = json!({"messages": [
            {"role": "user", "content": "key sk-abcdefghij1234567890 mail me at dev@example.com"}
        ]});
        let report = redact_body(&s, "sess", &mut body);
        assert_eq!(report.spans_redacted, 2);
        let text = body["messages"][0]["content"].as_str().unwrap();
        assert!(text.contains("__HR_SECRET_"), "got: {text}");
        assert!(text.contains("__HR_EMAIL_"), "got: {text}");
        unredact_body(&s, "sess", &mut body);
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("dev@example.com"));
    }

    #[test]
    fn relative_paths_need_two_slashes_or_an_extension() {
        let s = store();
        let mut body = json!({"messages": [
            {"role": "user", "content": "open src/main.py and a/b/c then"}
        ]});
        let report = redact_body(&s, "sess", &mut body);
        assert_eq!(report.spans_redacted, 2);
    }

    #[test]
    fn unknown_placeholders_stay_and_count_as_misses() {
        let s = store();
        let table = restore_table(&s, "sess");
        assert!(table.is_none(), "empty map means no pass at all");
        let s2 = store();
        let mut body = json!({"messages": [{"role": "user", "content": "/home/x/y.py"}]});
        redact_body(&s2, "sess", &mut body);
        let table = restore_table(&s2, "sess").unwrap();
        let (out, misses) = table.restore_bytes(b"echo __HR_PATH_9999__ done");
        assert_eq!(misses, 1);
        assert_eq!(out, b"echo __HR_PATH_9999__ done");
    }

    #[tokio::test]
    async fn stream_restore_survives_chunk_splits() {
        use futures_util::stream;
        let s = store();
        let mut body = json!({"messages": [{"role": "user", "content": "/home/ruben/a/very/long/path/file.py"}]});
        redact_body(&s, "sess", &mut body);
        let table = restore_table(&s, "sess").unwrap();
        // Split the placeholder stream into awkward pieces — including inside
        // the uncounted home token.
        let full = "data: {\"t\": \"__HR_HOME__/a/very/long/path/file.py\"}\n";
        let bytes = full.as_bytes();
        let chunks: Vec<Result<Bytes, std::io::Error>> = bytes
            .chunks(3)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let mut adapted = restore_stream(stream::iter(chunks), table);
        let mut out = Vec::new();
        while let Some(chunk) = adapted.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "data: {\"t\": \"/home/ruben/a/very/long/path/file.py\"}\n"
        );
    }

    /// The tail hides the inner end from the consumer for one poll, so a
    /// second poll must not touch the terminated inner. The CCR rewrite
    /// downstream is an `Unfold`, which panics on a post-`None` poll —
    /// that panic killed the worker and truncated every redacted routed
    /// stream mid-frame. `stream::iter` tolerates the extra poll, so only
    /// an `Unfold` inner reproduces it.
    #[tokio::test]
    async fn stream_restore_never_polls_a_terminated_inner() {
        use futures_util::stream;
        let s = store();
        let mut body = json!({"messages": [{"role": "user", "content": "/home/ruben/a/very/long/path/file.py"}]});
        redact_body(&s, "sess", &mut body);
        let table = restore_table(&s, "sess").unwrap();
        let full = "data: {\"t\": \"__HR_HOME__/a/very/long/path/file.py\"}\n";
        let chunks: Vec<Bytes> = full
            .as_bytes()
            .chunks(7)
            .map(Bytes::copy_from_slice)
            .collect();
        let inner = stream::unfold(Some(chunks), |state| async move {
            match state {
                Some(mut v) if !v.is_empty() => {
                    let head = v.remove(0);
                    Some((Ok::<_, std::io::Error>(head), Some(v)))
                }
                _ => None,
            }
        });
        let mut adapted = restore_stream(inner, table);
        let mut out = Vec::new();
        while let Some(chunk) = adapted.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "data: {\"t\": \"/home/ruben/a/very/long/path/file.py\"}\n"
        );
        // Drive past the end: these polls reached into the terminated
        // `Unfold` before the fix and panicked the worker.
        for _ in 0..3 {
            assert!(adapted.next().await.is_none());
        }
    }

    fn redact_one(input: &str) -> (String, RedactStore) {
        let s = store();
        let mut body = json!({"messages": [{"role": "user", "content": input}]});
        redact_body(&s, "sess", &mut body);
        (
            body["messages"][0]["content"].as_str().unwrap().to_string(),
            s,
        )
    }
    #[test]
    fn pgpass_hides_only_the_password() {
        let (text, s) = redact_one("db.internal:5432:mydb:admin:s3cr3tP@ssw0rd");
        let token = text
            .strip_prefix("db.internal:5432:mydb:admin:")
            .expect("prefix stays");
        assert_secret_token(token, &text);
        unredact_check(
            &s,
            "sess",
            &text,
            "db.internal:5432:mydb:admin:s3cr3tP@ssw0rd",
        );
    }

    /// `__HR_<KIND>_<16 hex>__` shape check shared by the opaque-token tests.
    fn assert_token_shape(token: &str, kind: &str, text: &str) {
        let prefix = format!("__HR_{kind}_");
        assert!(
            token.starts_with(&prefix) && token.ends_with("__"),
            "got: {text}"
        );
        let hex = &token[prefix.len()..token.len() - 2];
        assert_eq!(hex.len(), 16, "64-bit suffix: {text}");
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "got: {text}");
    }

    fn assert_secret_token(token: &str, text: &str) {
        assert_token_shape(token, "SECRET", text);
    }

    fn unredact_check(store: &RedactStore, session: &str, redacted: &str, expected: &str) {
        let mut body = json!({"messages": [{"role": "user", "content": redacted}]});
        unredact_body(store, session, &mut body);
        assert_eq!(body["messages"][0]["content"].as_str().unwrap(), expected);
    }

    #[test]
    fn postgres_url_hides_only_the_password() {
        let input = "postgres://admin:s3cr3tP@ssw0rd@db.internal:5432/mydb";
        let (text, s) = redact_one(input);
        let token = text
            .strip_prefix("postgres://admin:")
            .expect("prefix stays")
            .strip_suffix("@db.internal:5432/mydb")
            .expect("suffix stays");
        assert_secret_token(token, &text);
        unredact_check(&s, "sess", &text, input);
    }

    #[test]
    fn credential_names_cover_the_usual_suspects() {
        let input = "password=hunter2hunter secret_access_key = wJalrXUtnFEMI7K7MDENG7bPxRfiCYEXAMPLEKEY aws_session_token=FQoGZXIvYXdzENz//////////wEaDKxOPFbyg";
        let (text, s) = redact_one(input);
        assert!(!text.contains("hunter2hunter"), "got: {text}");
        assert!(!text.contains("wJalrXUtnFEMI"), "got: {text}");
        assert!(!text.contains("FQoGZXIv"), "got: {text}");
        assert!(text.contains("password="), "name stays: {text}");
        assert!(text.contains("secret_access_key = "), "name stays: {text}");
        unredact_check(&s, "sess", &text, input);
    }

    #[test]
    fn known_key_prefixes() {
        let (text, _) = redact_one(
            "keys ASIAIOSFODNN7EXAMPLE glpat-AbCdEfGhIjKlMnOpQrSt hf_zyxwvutsrqponmlkjih",
        );
        assert!(!text.contains("ASIAIOSFODNN7EXAMPLE"), "got: {text}");
        assert!(!text.contains("glpat-AbCdEfGhIjKlMnOpQrSt"), "got: {text}");
        assert!(!text.contains("hf_zyxwvutsrqponmlkjih"), "got: {text}");
    }

    #[test]
    fn ssh_user_at_host_goes_but_matrix_code_stays() {
        let (text, _) = redact_one("ssh deploy@db-primary and root@localhost, but C = A@B");
        assert!(!text.contains("deploy@db-primary"), "got: {text}");
        assert!(!text.contains("root@localhost"), "got: {text}");
        assert!(text.contains("A@B"), "matrix code stays: {text}");
    }

    #[test]
    fn arn_account_id_goes() {
        let input = "arn:aws:ssm:us-east-1:123456789012:parameter/db-pass";
        let (text, s) = redact_one(input);
        let token = text
            .strip_prefix("arn:aws:ssm:us-east-1:")
            .expect("prefix stays")
            .strip_suffix(":parameter/db-pass")
            .expect("suffix stays");
        assert_secret_token(token, &text);
        unredact_check(&s, "sess", &text, input);
    }

    #[test]
    fn pem_blocks_go_whole() {
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmU=\n-----END OPENSSH PRIVATE KEY-----";
        let (text, s) = redact_one(key);
        assert_secret_token(&text, &text);
        unredact_check(&s, "sess", &text, key);
    }

    #[test]
    fn filenames_are_paths_not_secrets() {
        let (text, _) = redact_one("see quarterly-report-final-2024.pdf attached");
        assert_eq!(
            text, "see quarterly-report-final-2024.pdf attached",
            "got: {text}"
        );
    }

    /// Restart simulation: a fresh store mints the identical token, so the
    /// provider's cached prefix survives a proxy restart with no recache —
    /// and the rebuilt map still restores.
    #[test]
    fn tokens_survive_a_store_restart() {
        let input = "key sk-abcdefghij1234567890 in /home/ruben/x/y.py";
        let (before, _) = redact_one(input);
        // Fresh process state, same session.
        let after_store = store();
        let mut body = json!({"messages": [{"role": "user", "content": input}]});
        redact_body(&after_store, "sess", &mut body);
        let after = body["messages"][0]["content"].as_str().unwrap();
        assert_eq!(before, after, "restart must not rotate tokens");
        unredact_check(&after_store, "sess", after, input);
    }

    /// Same value in two sessions mints two mutually opaque tokens: the
    /// session key salts the hash, so sessions cannot be correlated upstream.
    #[test]
    fn tokens_are_session_scoped() {
        let s = store();
        let mut a = json!({"messages": [{"role": "user", "content": "sk-abcdefghij1234567890"}]});
        let mut b = a.clone();
        redact_body(&s, "sess-a", &mut a);
        redact_body(&s, "sess-b", &mut b);
        let ta = a["messages"][0]["content"].as_str().unwrap();
        let tb = b["messages"][0]["content"].as_str().unwrap();
        assert_ne!(ta, tb, "sessions must not share tokens");
        unredact_check(&s, "sess-a", ta, "sk-abcdefghij1234567890");
    }
}
