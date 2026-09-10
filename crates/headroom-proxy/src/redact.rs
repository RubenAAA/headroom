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

use axum::response::Response;
use bytes::Bytes;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use futures_util::Stream;
use lru::LruCache;
use serde_json::Value;

/// Sessions remembered. Oldest goes when full; its placeholders then stop
/// restoring, which is loud (a miss counter) rather than silent.
const STORE_CAPACITY: usize = 128;
/// Placeholders per session. Same deal: eviction orphans ancient history, and
/// the miss counter says so.
const SESSION_CAPACITY: usize = 512;
/// Hard ceiling on the streaming hold-back. It has to cover the longest token
/// a chunk edge can split, or the two halves are emitted unrestored — which is
/// the corruption this module exists to prevent.
const MAX_OVERLAP: usize = MAX_TOKEN_LEN;
/// Placeholders kept for restore regardless of which session minted them.
/// Sized well past the per-session cap because a model may quote a path many
/// turns after it first saw it, and because subagent fan-out mints under a new
/// session key every time — see `RedactStore::remember`.
const GLOBAL_RESTORE_CAPACITY: usize = 32_768;
/// Hex characters a counted token may carry. A token holds nonce + ciphertext
/// + tag, so its length tracks the value it replaced; this bounds the value at
/// ~1 KB, well past any path or credential the scanners match.
const MAX_TOKEN_HEX: usize = 2048;
/// The longest a whole placeholder can be: prefix, kind, the hex body, and the
/// closing `__`. The streaming restore holds back this much at a chunk edge.
const MAX_TOKEN_LEN: usize = PREFIX.len() + 6 + 1 + MAX_TOKEN_HEX + 2;

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

/// Bytes of nonce carried at the front of every counted token.
const NONCE_LEN: usize = 12;
/// Nonce + Poly1305 tag: the fixed cost of a self-inverting token.
const TOKEN_OVERHEAD: usize = NONCE_LEN + 16;

/// Where the restore key lives. `HEADROOM_REDACT_KEY_FILE` overrides it, which
/// is what the tests use so a run never touches the real one.
fn key_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("HEADROOM_REDACT_KEY_FILE") {
        return std::path::PathBuf::from(p);
    }
    let state = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| s.starts_with('/'))
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            format!("{home}/.local/state")
        });
    std::path::PathBuf::from(state)
        .join("headroom")
        .join("redact.key")
}

/// Read the key, or create one on first use. `0600`, and `create_new` so two
/// proxies racing to start cannot write over each other — the loser re-reads
/// the winner's key rather than minting tokens the winner cannot restore.
///
/// A key that cannot be read or written is not fatal: the process falls back
/// to a random in-memory key, which restores everything it minted itself and
/// nothing from before it, exactly as the old map-only design did.
fn load_or_create_key() -> ([u8; 32], bool) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let path = key_path();
    let mut key = [0u8; 32];

    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == 32 {
            key.copy_from_slice(&bytes);
            return (key, true);
        }
        tracing::warn!(
            event = "redact_key_malformed",
            path = %path.display(),
            len = bytes.len(),
            "restore key is not 32 bytes; placeholders minted before now will not restore"
        );
    }

    getrandom_key(&mut key);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(&key) {
                tracing::warn!(event = "redact_key_write_failed", error = %e);
                return (key, false);
            }
            tracing::info!(
                event = "redact_key_created",
                path = %path.display(),
                "wrote a new restore key"
            );
            (key, true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Lost the race. The winner's key is the one that counts.
            match std::fs::read(&path) {
                Ok(bytes) if bytes.len() == 32 => {
                    key.copy_from_slice(&bytes);
                    (key, true)
                }
                _ => (key, false),
            }
        }
        Err(e) => {
            tracing::warn!(
                event = "redact_key_unavailable",
                path = %path.display(),
                error = %e,
                "no restore key on disk; placeholders will not survive this process"
            );
            (key, false)
        }
    }
}

/// 32 bytes from the OS. `/dev/urandom` directly rather than through an RNG
/// crate: one read, no generic plumbing, and the same source either way.
fn getrandom_key(out: &mut [u8; 32]) {
    use std::io::Read;
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(out)) {
        Ok(()) => {}
        Err(e) => {
            // Never mint tokens from a predictable key: without randomness the
            // placeholder is guessable, which is the one thing it must not be.
            panic!("cannot read /dev/urandom for the redaction key: {e}");
        }
    }
}

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

    fn placeholder(
        &mut self,
        cipher: &ChaCha20Poly1305,
        kind: &'static str,
        original: &str,
        session: &str,
    ) -> String {
        if let Some(existing) = self.reverse.get(original) {
            return existing.clone();
        }
        // 64-bit truncation: a collision would restore the wrong secret, so
        // on the paranoia that two values in one session ever collide, salt
        // in an attempt counter rather than aliasing.
        let mut attempt = 0u32;
        let token = loop {
            let candidate = token_for(cipher, session, kind, original, attempt);
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
    /// Every placeholder this process has minted, whatever session it belongs
    /// to. The per-session maps decide what gets *redacted*; this one decides
    /// what can be *restored*, and the two are not the same question.
    ///
    /// A token is a hash of `(session, kind, value)`, so it is opaque upstream
    /// and unique here — restoring one from this map can only ever return the
    /// value that minted it. What it buys: a placeholder still resolves after
    /// its session was evicted, after the session key drifts mid-conversation,
    /// and when a subagent's text is quoted back in another session. Each of
    /// those used to reach the client as a literal `__HR_PATH_<hex>__`; one of
    /// them wrote such a token into a settings file on 2026-09-10, where it
    /// read as a path and broke the hook it replaced.
    global: Arc<Mutex<LruCache<String, String>>>,
    /// The key behind every counted token. A token carries its own ciphertext,
    /// so restore is decryption rather than a lookup — see [`token_for`].
    cipher: Arc<ChaCha20Poly1305>,
    /// Whether that key came from disk. False means this process invented one
    /// and nothing it mints will outlive it.
    key_is_durable: bool,
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
        let global = std::num::NonZeroUsize::new(GLOBAL_RESTORE_CAPACITY)
            .unwrap_or(std::num::NonZeroUsize::MIN);
        let (key, durable) = load_or_create_key();
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(capacity))),
            global: Arc::new(Mutex::new(LruCache::new(global))),
            cipher: Arc::new(ChaCha20Poly1305::new(Key::from_slice(&key))),
            key_is_durable: durable,
        }
    }

    /// A store on an explicit key. Tests use it so a run neither reads nor
    /// writes the machine's real key.
    pub fn with_key(key: [u8; 32]) -> Self {
        let capacity =
            std::num::NonZeroUsize::new(STORE_CAPACITY).unwrap_or(std::num::NonZeroUsize::MIN);
        let global = std::num::NonZeroUsize::new(GLOBAL_RESTORE_CAPACITY)
            .unwrap_or(std::num::NonZeroUsize::MIN);
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(capacity))),
            global: Arc::new(Mutex::new(LruCache::new(global))),
            cipher: Arc::new(ChaCha20Poly1305::new(Key::from_slice(&key))),
            key_is_durable: true,
        }
    }

    /// Whether placeholders minted now will restore after a restart.
    pub fn key_is_durable(&self) -> bool {
        self.key_is_durable
    }

    fn with_session(&self, session_key: &str, f: impl FnOnce(&mut SessionMap)) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let map = guard.get_or_insert_mut(session_key.to_string(), SessionMap::new);
        f(map);
    }

    /// Record a minted placeholder for restore, outside its session.
    fn remember(&self, token: &str, original: &str) {
        let mut guard = match self.global.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.put(token.to_string(), original.to_string());
    }

    /// Whether anything has been minted at all. Cheap check so a process that
    /// never redacts never pays for a restore pass.
    fn recall_is_empty(&self) -> bool {
        let guard = match self.global.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.is_empty()
    }

    /// The value behind a placeholder, whichever session minted it.
    fn recall(&self, token: &str) -> Option<String> {
        let mut guard = match self.global.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(token).cloned()
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
    redact_body_with_home(store, session_key, body, None)
}

/// [`redact_body`] with an explicit home override. Production passes `None`
/// (ambient `$HOME`); tests pass `Some` so home-asserting tests run on any
/// machine instead of only where the fixtures were written.
pub fn redact_body_with_home(
    store: &RedactStore,
    session_key: &str,
    body: &mut Value,
    home: Option<&str>,
) -> RedactReport {
    let mut redactor = BodyRedactor::with_home(store, session_key, home);
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
    walk_strings(body, &mut |s| {
        let (out, _) = restore_str(store, &forward, s);
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
        Self::with_home(store, session_key, None)
    }

    /// Explicit home for tests: the ambient `$HOME` makes home-asserting
    /// tests pass only on the machine they were written on. Production
    /// always passes `None` (ambient); tests pass `Some` and stay
    /// machine-independent.
    fn with_home(store: &'a RedactStore, session_key: &str, home: Option<&str>) -> Self {
        Self {
            store,
            session_key: session_key.to_string(),
            home: home
                .map(str::to_string)
                .or_else(|| std::env::var("HOME").ok())
                .filter(|h| h.starts_with('/')),
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
            // Already-redacted spans pass through untouched. Continuation
            // content can arrive redacted (cold-tier recoveries rejoin
            // post-redact); wrapping those spans in fresh tokens would point
            // the map at placeholders instead of originals and rotate the
            // bytes the provider caches. The home token travels with the
            // clear suffix it was minted with, consumed whole — the path
            // scanner below would otherwise swallow that suffix as a fresh
            // opaque path.
            if let Some(end) = self.placeholder_end(bytes, i) {
                out.push_str(&text[i..end]);
                i = end;
                continue;
            }
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
                        self.store.remember(&token, &home);
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
        let cipher = Arc::clone(&self.store.cipher);
        self.store.with_session(&session, |map| {
            token = map.placeholder(&cipher, kind, original, &session);
        });
        self.store.remember(&token, original);
        token
    }

    /// End index of an already-redacted span starting at `i`, if any. The
    /// counted tokens are opaque and consumed whole; the home token keeps
    /// the clear suffix it was minted with (the same path-char run the
    /// mint pass consumed), so the suffix is not re-read as a new path.
    /// Slicing stays on char boundaries: every byte consumed here is ASCII.
    fn placeholder_end(&self, bytes: &[u8], i: usize) -> Option<usize> {
        let (token, len) = parse_token(bytes, i)?;
        let mut end = i + len;
        if token == HOME_TOKEN {
            while end < bytes.len() && is_path_char(bytes[end]) {
                end += 1;
            }
        }
        Some(end)
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
                if let Some((start, len)) =
                    match_named_value(rest, name.len(), name.ends_with(b" "))
                {
                    return Some((start, len, SECRET_KIND));
                }
            }
        }
        // Compound credential names (`private_key`, `api-key`, `myKey`):
        // "key" alone is too generic to list (dict keys, prose), but
        // completing a `_`/`-` compound or a camel hump is a credential
        // name. The joiner requirement keeps `monkey`/`donkey` out:
        // alphanumerics before "key" never match.
        if i > 0 && starts_word_insensitive(rest, b"key") {
            let prev = bytes[i - 1];
            if prev == b'_' || prev == b'-' || (prev.is_ascii_lowercase() && rest[0] == b'K') {
                if let Some((start, len)) = match_named_value(rest, 3, false) {
                    return Some((start, len, SECRET_KIND));
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
            if len < 3
                || run.iter().all(|&b| b == b'/')
                || (!run[1..].contains(&b'/') && !has_extension(run))
            {
                // Single short segment (`/x`), bare root, or a run of nothing
                // but slashes (`//`, `///`) — not a path worth hiding.
                return None;
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
fn token_for(
    cipher: &ChaCha20Poly1305,
    session: &str,
    kind: &str,
    original: &str,
    attempt: u32,
) -> String {
    use sha2::{Digest, Sha256};
    // The nonce is derived from the value, so the same value in the same
    // session always mints the same token — the property the provider's cached
    // prefix depends on. Deriving it from the plaintext also means a nonce is
    // only ever paired with the plaintext that produced it, which is the rule
    // that makes reuse safe. The session is salted in so two sessions holding
    // the same secret still send different bytes upstream.
    let mut hasher = Sha256::new();
    hasher.update(b"headroom-redact-nonce");
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
    let nonce = Nonce::from_slice(&digest[..NONCE_LEN]);

    // The kind is authenticated but not encrypted: it is already in the token,
    // and binding it stops a token being read back as another kind.
    let sealed = cipher
        .encrypt(
            nonce,
            Payload {
                msg: original.as_bytes(),
                aad: kind.as_bytes(),
            },
        )
        .expect("ChaCha20-Poly1305 seals any plaintext this scanner can produce");

    let mut body = Vec::with_capacity(NONCE_LEN + sealed.len());
    body.extend_from_slice(&digest[..NONCE_LEN]);
    body.extend_from_slice(&sealed);
    format!("{PREFIX}{kind}_{}__", hex::encode(body))
}

/// Recover what a counted token carries. `None` when the token was not minted
/// by this key — an older process's, a shorter legacy token, or an invention.
fn value_of(cipher: &ChaCha20Poly1305, token: &str) -> Option<String> {
    let rest = token.strip_prefix(PREFIX)?.strip_suffix("__")?;
    let (kind, hex_body) = rest.split_once('_')?;
    let body = hex::decode(hex_body).ok()?;
    if body.len() <= TOKEN_OVERHEAD {
        return None;
    }
    let (nonce, sealed) = body.split_at(NONCE_LEN);
    let opened = cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: sealed,
                aad: kind.as_bytes(),
            },
        )
        .ok()?;
    String::from_utf8(opened).ok()
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
///
/// The snapshot answers every hit without a lock. A token the session never
/// minted falls through to `global`, which does take one — rare by
/// construction, since the common case is a session restoring its own.
pub struct RestoreTable {
    forward: HashMap<String, String>,
    max_len: usize,
    global: RedactStore,
}

/// Copy the session's map out.
///
/// Always `Some`. It used to return `None` when this process had redacted
/// nothing, on the reasoning that there was then nothing to restore — but a
/// token carries its own ciphertext now, so one minted by a previous process
/// can appear in a body this one never touched, and the key still opens it.
/// The cost is a scan for `__HR_` over responses that hold none; the scan is a
/// single pass and the alternative is the class of miss this design removes.
pub fn restore_table(store: &RedactStore, session_key: &str) -> Option<RestoreTable> {
    let forward = forward_snapshot(store, session_key);
    // Hold back a full token's worth even when this session's map is short or
    // empty: the token that needs restoring may have been minted elsewhere,
    // and a straddle has to survive the cut either way.
    let max_len = forward
        .keys()
        .map(|k| k.len())
        .max()
        .unwrap_or(0)
        .max(MAX_TOKEN_LEN);
    Some(RestoreTable {
        forward,
        max_len,
        global: store.clone(),
    })
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
                if let Some(original) = value_of(&self.global.cipher, token) {
                    // The token carries its own value. No map, no session, no
                    // process lifetime: this is the path that cannot miss.
                    out.extend_from_slice(original.as_bytes());
                } else if let Some(original) = self.forward.get(token) {
                    out.extend_from_slice(original.as_bytes());
                } else if let Some(original) = self.global.recall(token) {
                    out.extend_from_slice(original.as_bytes());
                } else if token == HOME_TOKEN {
                    // Home is whatever this machine's home is — knowable
                    // without any memory of having minted it.
                    match std::env::var("HOME") {
                        Ok(home) if home.starts_with('/') => out.extend_from_slice(home.as_bytes()),
                        _ => {
                            misses += 1;
                            crate::observability::redact_metrics::observe_restore_miss();
                            out.extend_from_slice(b"[headroom: unresolved ");
                            out.extend_from_slice(token.as_bytes());
                            out.extend_from_slice(b"]");
                        }
                    }
                } else {
                    // Nothing in the process knows this token. Emitting it raw
                    // is what put a placeholder into a settings file as if it
                    // were a filename: `/home/you/` restores, the tail does
                    // not, and the result is a path-shaped string that resolves
                    // to nothing. The marker cannot be mistaken for one — it
                    // holds spaces and brackets — while keeping the token
                    // visible for the client-side guard and for grep.
                    misses += 1;
                    crate::observability::redact_metrics::observe_restore_miss();
                    out.extend_from_slice(b"[headroom: unresolved ");
                    out.extend_from_slice(token.as_bytes());
                    out.extend_from_slice(b"]");
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

fn restore_str(store: &RedactStore, forward: &HashMap<String, String>, s: &str) -> (String, usize) {
    let table = RestoreTable {
        max_len: 0,
        forward: forward.clone(),
        global: store.clone(),
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
        // Counted form: `_` plus hex plus `__`. The bound is generous now that
        // a token carries a whole ciphertext, but it still exists: a bare
        // `__HR_PATH_` written in prose must not eat the rest of the line.
        j += 1;
        let hex_start = j;
        while j < rest.len() && rest[j].is_ascii_hexdigit() && j - hex_start < MAX_TOKEN_HEX {
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
                let emit_until = hold_from(&pending, overlap).unwrap_or(pending.len());
                let (out, misses) = table.restore_bytes(&pending[..emit_until]);
                if misses > 0 {
                    tracing::warn!(
                        event = "redact_restore_miss",
                        misses,
                        "placeholders no session could restore; emitted as unresolved markers"
                    );
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
                    tracing::warn!(
                        event = "redact_restore_miss",
                        misses,
                        "placeholders no session could restore; emitted as unresolved markers"
                    );
                }
                std::task::Poll::Ready(Some(Ok(Bytes::from(out))))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    })
}

// ---------------------------------------------------------------------------
// The seam every routed client shares
// ---------------------------------------------------------------------------

/// One turn's redaction, held between the outbound body and the response.
///
/// Redaction used to be wired per route: the translate path called
/// `redact_body` and then restored at each of its own response arms, and the
/// Cursor path — a different transport, added later — simply never called any
/// of it, so `--redact-sensitive` silently did nothing there. A promise the
/// flag makes for one client and not another is worse than no promise, and
/// nothing about restoring bytes is route-specific. So the seam lives here:
/// [`seam_outbound`] before a route sees the body, [`restore_response`] on
/// whatever it hands back.
pub struct Seam {
    store: RedactStore,
    session_key: String,
    /// Whether the outbound body held spans. A clean turn keeps the
    /// byte-identical wire body (no re-serialization churn on the provider
    /// prefix) but still restores: the response may quote placeholders
    /// minted by earlier turns.
    spans_redacted: usize,
}

/// Redact an outbound body on the way into any route. `None` only when the
/// flag is off. A clean body still returns a seam: the response may quote
/// placeholders minted by earlier turns, and those must not reach the
/// client as raw `__HR_*__` (a quoted token in a file or a second turn reads
/// as a literal and breaks, the settings-file class this stream fixed).
pub fn seam_outbound(
    store: &RedactStore,
    session_key: &str,
    parsed: &mut Value,
    enabled: bool,
) -> Option<Seam> {
    if !enabled {
        return None;
    }
    let report = redact_body(store, session_key, parsed);
    if report.spans_redacted > 0 {
        tracing::info!(
            event = "redact_outbound",
            spans_redacted = report.spans_redacted,
            placeholders_live = report.placeholders_live,
            "redacted sensitive spans before the route sent them"
        );
    }
    Some(Seam {
        store: store.clone(),
        session_key: session_key.to_string(),
        spans_redacted: report.spans_redacted,
    })
}

/// Restore placeholders in whatever a route returns.
///
/// Buffered or streaming makes no difference: the restore pass is byte-level
/// and chunk-boundary safe, so wrapping the response body covers a one-shot
/// JSON reply and an SSE stream with the same code. That is the whole reason
/// a route no longer needs to know redaction exists.
pub fn restore_response(seam: Option<Seam>, response: Response) -> Response {
    let Some(seam) = seam else { return response };
    let Some(table) = restore_table(&seam.store, &seam.session_key) else {
        return response;
    };
    let (parts, body) = response.into_parts();
    let restored = restore_stream(body.into_data_stream(), table);
    Response::from_parts(parts, axum::body::Body::from_stream(restored))
}

/// One shared helper for every route that forwards a buffered client body.
///
/// A route builds the gate once per request from pieces `AppState` already
/// carries, runs the body through [`RedactGate::seam_bytes`] before it
/// leaves, and wraps whatever comes back with [`restore_response`].
/// Encrypt and decrypt stay paired inside one request: the session key only
/// has to be stable for that request, because counted tokens decrypt from
/// the process key rather than from session memory.
///
/// When the flag is off — or the body is not JSON — `seam_bytes` hands the
/// bytes back untouched with no seam, and `restore_response` passes the
/// response through. A clean JSON body keeps its bytes but still restores:
/// the response may quote placeholders minted by earlier turns. Routes that
/// stream the body without buffering cannot use this without changing their
/// shape, so they stay out by construction.
pub struct RedactGate {
    enabled: bool,
    store: RedactStore,
    session_key: String,
}

impl RedactGate {
    /// Bundle the flag, the store, and this request's session key once.
    pub fn new(enabled: bool, store: &RedactStore, session_key: &str) -> Self {
        Self {
            enabled,
            store: store.clone(),
            session_key: session_key.to_string(),
        }
    }

    /// Redact a buffered body on its way out. Returns the bytes to send and
    /// the seam to restore the response with. Any body this cannot parse as
    /// JSON — or cannot re-serialize after redaction — goes through
    /// byte-equal with no seam. A clean turn keeps its original bytes (no
    /// re-serialization churn on the provider prefix) but still restores.
    pub fn seam_bytes(&self, body: Bytes) -> (Bytes, Option<Seam>) {
        if !self.enabled {
            return (body, None);
        }
        let mut parsed: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return (body, None),
        };
        let seam = seam_outbound(&self.store, &self.session_key, &mut parsed, true);
        let Some(seam) = seam else {
            return (body, None);
        };
        if seam.spans_redacted == 0 {
            return (body, Some(seam));
        }
        match serde_json::to_vec(&parsed) {
            Ok(redacted) => (Bytes::from(redacted), Some(seam)),
            Err(_) => (body, None),
        }
    }
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

/// A value in even space-separated groups — a Google app key arrives as four
/// blocks of four letters — is one secret, not four words. Extends the run at
/// `end` over further groups only while each matches the first in length and
/// character class, so a second assignment further along the line stops the
/// run, and prose never runs away.
fn extend_over_groups(rest: &[u8], start: usize, end: usize) -> usize {
    let first = &rest[start..end];
    let want_class = char_class(first);
    if first.len() < 3 || want_class.is_none() {
        return end;
    }
    let want_len = first.len();
    let mut j = end;
    for _ in 0..7 {
        if j >= rest.len() || rest[j] != b' ' {
            break;
        }
        let group_start = j + 1;
        let mut k = group_start;
        while k < rest.len() && is_token_char(rest[k]) {
            k += 1;
        }
        let group = &rest[group_start..k];
        if group.len() != want_len || char_class(group) != want_class {
            break;
        }
        j = k;
    }
    j
}

/// `Some` when every byte shares one class: lowercase, uppercase, or digits.
fn char_class(group: &[u8]) -> Option<u8> {
    let class = |b: u8| match b {
        b'a'..=b'z' => Some(b'a'),
        b'A'..=b'Z' => Some(b'A'),
        b'0'..=b'9' => Some(b'0'),
        _ => None,
    };
    let first = class(*group.first()?)?;
    group
        .iter()
        .all(|&b| class(b) == Some(first))
        .then_some(first)
}

fn starts_word_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

/// Value span after a credential name occupying the first `name_len` bytes
/// of `rest`: optional ident-char tail (`secret_key`), separator (`:`/`=`),
/// optional quotes, then the value. `bare_value` (for `Bearer <token>`)
/// skips the tail/separator scan. Returns `(offset, len)` of the value;
/// `None` when no separator follows or the value is too short to be a
/// secret (8+ chars — shorter strings are not worth the false-positive
/// rate, and the entropy arm cannot see them either).
fn match_named_value(rest: &[u8], name_len: usize, bare_value: bool) -> Option<(usize, usize)> {
    let mut j = name_len;
    let mut quote = None;
    if !bare_value {
        while j < rest.len() && is_ident_char(rest[j]) {
            j += 1;
        }
        while j < rest.len() && (rest[j] == b' ' || rest[j] == b'\t') {
            j += 1;
        }
        if j < rest.len() && (rest[j] == b':' || rest[j] == b'=') {
            j += 1;
        } else {
            return None;
        }
        while j < rest.len() && (rest[j] == b' ' || rest[j] == b'\t') {
            j += 1;
        }
        if j < rest.len() && (rest[j] == b'\'' || rest[j] == b'"') {
            quote = Some(rest[j]);
            j += 1;
        }
    }
    let start = j;
    // Already redacted: a placeholder as the value means there is no
    // cleartext secret here, so minting again would only wrap opacity in
    // opacity (and single-pass restore would stop at the inner token).
    // Leave it for `placeholder_end` to consume whole downstream.
    if parse_token(rest, start).is_some() {
        return None;
    }
    if let Some(q) = quote {
        // A quoted value runs to its closing quote.
        while j < rest.len() && rest[j] != q && rest[j] != b'\n' {
            j += 1;
        }
    } else {
        while j < rest.len() && is_token_char(rest[j]) {
            j += 1;
        }
        j = extend_over_groups(rest, start, j);
    }
    if j - start >= 8 {
        Some((start, j - start))
    } else {
        None
    }
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
/// Where a chunk has to stop, so a token is never split across two emits.
///
/// Holds back only what could still become a token — an unterminated `__HR_`,
/// or a partial prefix sitting at the very end — rather than a fixed tail on
/// every chunk. That matters now that a token carries its own ciphertext and
/// can run to a kilobyte: a blanket hold-back that size would stall an SSE
/// stream by a kilobyte at a time, while this one usually holds nothing.
///
/// `window` bounds how far back a candidate is honoured. A bare `__HR_` in
/// prose never terminates, and without the bound it would hold the stream to
/// the end; past `window` bytes it is prose, and prose can be emitted.
fn hold_from(pending: &[u8], window: usize) -> Option<usize> {
    let needle = PREFIX.as_bytes();

    // Where the last complete token ends. Nothing before this can be a hold
    // point: a token's own closing `__` reads as the start of another prefix,
    // and holding there would cut the token this pass is about to restore.
    let mut settled = 0;
    let mut i = 0;
    while i < pending.len() {
        match parse_token(pending, i) {
            Some((_, len)) => {
                i += len;
                settled = i;
            }
            None => i += 1,
        }
    }

    // An unterminated token start within the window.
    let floor = pending.len().saturating_sub(window).max(settled);
    let open = (floor..pending.len().saturating_sub(needle.len() - 1))
        .rev()
        .find(|&i| pending[i..].starts_with(needle))
        .filter(|&i| parse_token(pending, i).is_none());

    // A prefix cut in half by the chunk edge (`…__H`).
    let partial = (1..needle.len())
        .rev()
        .find(|&k| {
            pending.len() >= k
                && pending.len() - k >= settled
                && pending[pending.len() - k..] == needle[..k]
        })
        .map(|k| pending.len() - k);

    match (open, partial) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
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

    /// A store on a fixed test key. Never `RedactStore::new()`: that reads or
    /// creates the machine's real key file, so a test run would leave state on
    /// the box and its results would depend on what a previous run wrote.
    fn store() -> RedactStore {
        RedactStore::with_key([0xA5; 32])
    }

    /// Fixed home for every test below, so home-asserting tests run on any
    /// machine. The fixtures used to name the author's own home, which made
    /// them pass only where they were written -- and shipped a personal
    /// path in every checkout.
    const TEST_HOME: &str = "/home/testuser";

    fn redact_body(store: &RedactStore, session_key: &str, body: &mut Value) -> RedactReport {
        super::redact_body_with_home(store, session_key, body, Some(TEST_HOME))
    }

    #[test]
    fn home_paths_round_trip() {
        let s = store();
        let mut body = json!({"messages": [
            {"role": "user", "content": "read /home/testuser/headroom/src/main.rs please"}
        ]});
        let report = redact_body(&s, "sess", &mut body);
        assert_eq!(report.spans_redacted, 1);
        let text = body["messages"][0]["content"].as_str().unwrap();
        // Prefix hidden, structure clear.
        assert_eq!(text, "read __HR_HOME__/headroom/src/main.rs please");
        unredact_body(&s, "sess", &mut body);
        assert_eq!(
            body["messages"][0]["content"].as_str().unwrap(),
            "read /home/testuser/headroom/src/main.rs please"
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
        let mut body = json!({"messages": [{"role": "user", "content": "/home/testuser/a.py"}]});
        redact_body(&s, "sess", &mut body);
        let table = restore_table(&s, "sess").unwrap();
        let (out, misses) = table.restore_bytes(b"run __HR_FOO_0001__ and __HR_PATH_9999__");
        assert_eq!(misses, 1, "unknown kind is bytes, unknown number is a miss");
        assert_eq!(
            out, b"run __HR_FOO_0001__ and [headroom: unresolved __HR_PATH_9999__]",
            "an unresolvable token is marked, never passed through as itself"
        );
    }

    #[test]
    fn same_path_keeps_one_placeholder_across_turns() {
        let s = store();
        let mut a = json!({"messages": [{"role": "user", "content": "/home/testuser/a.py"}]});
        let mut b = json!({"messages": [{"role": "user", "content": "fix /home/testuser/a.py"}]});
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
                 "input": {"file_path": "/home/testuser/secret/keys.txt"}}
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
            br#"{"file_path": "/home/testuser/secret/keys.txt"}"#.as_slice()
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

    /// Compound credential names (`private_key`, `api-key`, `myKey`) redact
    /// short values that no prefix and no entropy rule can see.
    #[test]
    fn compound_key_names_redact() {
        for content in [
            "private_key = hunter2hunter",
            "api-key: hunter2hunter",
            "myKey=hunter2hunter",
            "encryption_key = hunter2hunter",
        ] {
            let s = store();
            let mut body = json!({"messages": [{"role": "user", "content": content}]});
            let report = redact_body(&s, "sess", &mut body);
            assert_eq!(report.spans_redacted, 1, "missed: {content}");
            let text = body["messages"][0]["content"].as_str().unwrap();
            assert!(text.contains("__HR_SECRET_"), "got: {text}");
            unredact_body(&s, "sess", &mut body);
            assert_eq!(
                body["messages"][0]["content"].as_str().unwrap(),
                content,
                "restore must be exact"
            );
        }
    }

    /// Keywords mid-identifier (`client_secret`, `my_token`,
    /// `aws_secret_access_key`) already matched before the compound rule;
    /// pin that so the refactor above cannot regress it.
    #[test]
    fn mid_identifier_keywords_still_match() {
        for content in [
            "client_secret = hunter2hunter",
            "my_token: hunter2hunter",
            "aws_secret_access_key = hunter2hunter",
            "apiSecret=hunter2hunter",
        ] {
            let s = store();
            let mut body = json!({"messages": [{"role": "user", "content": content}]});
            let report = redact_body(&s, "sess", &mut body);
            assert_eq!(report.spans_redacted, 1, "missed: {content}");
        }
    }

    /// The joiner requirement keeps prose out: alphanumerics before "key"
    /// (`monkey`, `donkey`) never match, however long the value.
    #[test]
    fn prose_key_words_do_not_match() {
        for content in [
            "the monkey = banana bread recipe here",
            "donkey: kongregate paper draft",
        ] {
            let s = store();
            let mut body = json!({"messages": [{"role": "user", "content": content}]});
            let report = redact_body(&s, "sess", &mut body);
            assert_eq!(report.spans_redacted, 0, "false positive: {content}");
        }
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
    fn bare_slashes_are_not_paths() {
        let s = store();
        let mut body = json!({"messages": [
            {"role": "user", "content": "//! doc\n/// doc\n// comment\nhttps://example.com/x stays"}
        ]});
        let report = redact_body(&s, "sess", &mut body);
        assert_eq!(
            report.spans_redacted,
            0,
            "got: {}",
            body["messages"][0]["content"].as_str().unwrap()
        );
    }

    /// A clean turn quoting a placeholder minted by an earlier turn must
    /// still restore: the token would otherwise reach the client as raw
    /// `__HR_*__` and land literally in files. The outbound wire body stays
    /// byte-identical (no re-serialization churn on the provider prefix).
    #[test]
    fn clean_turn_quoting_old_placeholder_still_restores() {
        let s = store();
        let mut first =
            json!({"messages": [{"role": "user", "content": "password = hunter2hunter"}]});
        let report = redact_body(&s, "sess", &mut first);
        assert_eq!(report.spans_redacted, 1);
        let token = first["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(token.contains("__HR_SECRET_"), "got: {token}");

        let gate = RedactGate::new(true, &s, "sess");
        let clean = Bytes::from(
            format!(r#"{{"messages":[{{"role":"user","content":"as you said: {token}"}}]}}"#)
                .into_bytes(),
        );
        let (outbound, seam) = gate.seam_bytes(clean.clone());
        assert_eq!(outbound, clean, "clean wire body must stay byte-identical");
        assert!(seam.is_some(), "clean turn still needs the restore seam");

        let table = restore_table(&s, "sess").unwrap();
        let (restored, misses) = table.restore_bytes(&outbound);
        assert_eq!(misses, 0);
        assert!(
            String::from_utf8_lossy(&restored).contains("password = hunter2hunter"),
            "got: {}",
            String::from_utf8_lossy(&restored)
        );
    }

    /// The guarantee the key file buys: a token minted by a process that is
    /// gone still restores. `fresh` shares nothing with `s` but the key —
    /// empty session maps, empty global map, no memory of the mint at all.
    #[test]
    fn a_placeholder_restores_in_a_process_that_never_minted_it() {
        let key = [42u8; 32];
        let s = RedactStore::with_key(key);
        let mut body = json!({"messages": [{"role": "user", "content":
            "Bearer sk-live-9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c"}]});
        redact_body(&s, "sess", &mut body);
        let masked = body["messages"][0]["content"].as_str().unwrap().to_string();
        assert!(masked.contains("__HR_SECRET_"), "got: {masked}");

        let fresh = RedactStore::with_key(key);
        let mut echoed = json!({"messages": [{"role": "user", "content": masked}]});
        unredact_body(&fresh, "some-later-session", &mut echoed);
        assert_eq!(
            echoed["messages"][0]["content"].as_str().unwrap(),
            "Bearer sk-live-9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c",
            "a restart must not orphan a placeholder"
        );
    }

    /// The other half: a different key restores nothing, rather than restoring
    /// the wrong thing. Losing the key file costs retrieval, never correctness.
    #[test]
    fn another_key_restores_nothing() {
        let s = RedactStore::with_key([1u8; 32]);
        let mut body = json!({"messages": [{"role": "user", "content":
            "Bearer sk-live-9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c"}]});
        redact_body(&s, "sess", &mut body);
        let masked = body["messages"][0]["content"].as_str().unwrap().to_string();

        let stranger = RedactStore::with_key([2u8; 32]);
        let table = restore_table(&stranger, "sess");
        let (out, misses) = match table {
            Some(t) => t.restore_bytes(masked.as_bytes()),
            None => (masked.as_bytes().to_vec(), 0),
        };
        let text = String::from_utf8_lossy(&out);
        assert!(
            !text.contains("sk-live"),
            "a stranger's key must not open it"
        );
        assert!(
            text.contains("[headroom: unresolved") || misses == 0,
            "an unopenable token is marked, never guessed: {text}"
        );
    }

    /// Home needs no key and no map: it is whatever this machine's home is.
    #[test]
    fn the_home_token_restores_from_the_environment() {
        let s = RedactStore::with_key([9u8; 32]);
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.starts_with('/') {
            return;
        }
        let table = RestoreTable {
            forward: HashMap::new(),
            max_len: MAX_TOKEN_LEN,
            global: s,
        };
        let (out, misses) = table.restore_bytes(b"cat __HR_HOME__/notes.md");
        assert_eq!(misses, 0);
        assert_eq!(
            String::from_utf8_lossy(&out),
            format!("cat {home}/notes.md")
        );
    }

    /// The bug that wrote a placeholder into a settings file, in one test.
    ///
    /// A body redacted twice used to have its own output swallowed: the second
    /// pass read `__HR_HOME__/.claude/hooks/review-gate.sh` as one path-shaped
    /// run and minted a fresh opaque token over it, so the value behind that
    /// token was itself redacted text. Restoring then took two passes to reach
    /// the real path, and any layer that missed left the client holding
    /// something path-shaped that resolved to nothing.
    #[test]
    fn redacting_twice_changes_nothing_the_first_pass_wrote() {
        let s = store();
        let mut body = json!({"messages": [{"role": "user", "content":
            "/home/testuser/.claude/hooks/review-gate.sh"}]});
        redact_body(&s, "sess", &mut body);
        let once = body["messages"][0]["content"].as_str().unwrap().to_string();
        redact_body(&s, "sess", &mut body);
        let twice = body["messages"][0]["content"].as_str().unwrap().to_string();

        assert_eq!(once, twice, "the second pass must find nothing left to do");
        assert_eq!(
            once, "__HR_HOME__/.claude/hooks/review-gate.sh",
            "a home-rooted path keeps its suffix in the clear"
        );
    }

    /// The same body, redacted twice, restored from a session that never
    /// minted any of it — the two halves of the failure together.
    #[test]
    fn a_twice_redacted_body_restores_whole_from_any_session() {
        let s = store();
        let mut body = json!({"messages": [{"role": "user", "content":
            "/home/testuser/.claude/hooks/review-gate.sh"}]});
        redact_body(&s, "sess", &mut body);
        redact_body(&s, "sess", &mut body);
        unredact_body(&s, "some-other-session", &mut body);
        assert_eq!(
            body["messages"][0]["content"].as_str().unwrap(),
            "/home/testuser/.claude/hooks/review-gate.sh"
        );
    }

    /// The shape that broke a settings file: a token no map can resolve must
    /// not reach the client looking like something a tool can use.
    #[test]
    fn unknown_placeholders_are_marked_and_count_as_misses() {
        // A store that has minted nothing still restores: the key opens tokens
        // an earlier process minted, which is the whole point of the key file.
        let s2 = store();
        let mut body = json!({"messages": [{"role": "user", "content": "/home/x/y.py"}]});
        redact_body(&s2, "sess", &mut body);
        let table = restore_table(&s2, "sess").unwrap();
        let (out, misses) = table.restore_bytes(b"echo __HR_PATH_9999__ done");
        assert_eq!(misses, 1);
        assert_eq!(out, b"echo [headroom: unresolved __HR_PATH_9999__] done");
        assert!(
            !String::from_utf8_lossy(&out).contains("echo __HR"),
            "the marker must break the token out of command position"
        );
    }

    /// A subagent mints under its own session key; its text gets quoted back
    /// under another. Before the global map, that restored to nothing and the
    /// raw token reached the client.
    #[test]
    fn a_placeholder_minted_in_one_session_restores_in_another() {
        let s = store();
        let mut body =
            json!({"messages": [{"role": "user", "content": "/home/alice/secrets/key.pem"}]});
        redact_body(&s, "subagent-session", &mut body);
        let text = body["messages"][0]["content"].as_str().unwrap().to_string();

        let mut echoed = json!({"messages": [{"role": "user", "content": text}]});
        unredact_body(&s, "a-different-session", &mut echoed);

        assert_eq!(
            echoed["messages"][0]["content"].as_str().unwrap(),
            "/home/alice/secrets/key.pem",
            "the value must come back whichever session asks"
        );
    }

    /// Eviction is the other way a session loses a token it minted.
    #[test]
    fn a_placeholder_survives_its_sessions_eviction() {
        let s = store();
        let mut body =
            json!({"messages": [{"role": "user", "content": "/home/alice/old/file.rs"}]});
        redact_body(&s, "sess-0", &mut body);
        let text = body["messages"][0]["content"].as_str().unwrap().to_string();

        // Push the minting session out of the LRU entirely.
        for i in 1..=(STORE_CAPACITY + 1) {
            let mut filler = json!({"messages": [{"role": "user", "content": "/home/alice/f.rs"}]});
            redact_body(&s, &format!("sess-{i}"), &mut filler);
        }

        let mut echoed = json!({"messages": [{"role": "user", "content": text}]});
        unredact_body(&s, "sess-0", &mut echoed);
        assert_eq!(
            echoed["messages"][0]["content"].as_str().unwrap(),
            "/home/alice/old/file.rs",
            "an evicted session's placeholders must still resolve"
        );
    }

    #[tokio::test]
    async fn stream_restore_survives_chunk_splits() {
        use futures_util::stream;
        let s = store();
        let mut body = json!({"messages": [{"role": "user", "content": "/home/testuser/a/very/long/path/file.py"}]});
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
            "data: {\"t\": \"/home/testuser/a/very/long/path/file.py\"}\n"
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
        let mut body = json!({"messages": [{"role": "user", "content": "/home/testuser/a/very/long/path/file.py"}]});
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
            "data: {\"t\": \"/home/testuser/a/very/long/path/file.py\"}\n"
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
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "got: {text}");
        assert_eq!(hex.len() % 2, 0, "whole bytes: {text}");
        assert!(
            hex.len() / 2 > TOKEN_OVERHEAD,
            "a token carries nonce, ciphertext and tag: {text}"
        );
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

    /// Second pass over redacted text must be byte-identical: continuation
    /// content rejoins post-redact, and wrapping placeholders in fresh
    /// tokens would point the map at placeholders instead of originals —
    /// restore would then hand the client a token, not the real value.
    #[test]
    fn re_redaction_is_idempotent() {
        let s = store();
        let input = "key sk-abcdefghij1234567890 in /home/testuser/a/b.py, mail dev@example.com";
        let mut body = json!({"messages": [{"role": "user", "content": input}]});
        let first = redact_body(&s, "sess", &mut body);
        assert_eq!(first.spans_redacted, 3);
        let once = body["messages"][0]["content"].as_str().unwrap().to_string();
        let second = redact_body(&s, "sess", &mut body);
        assert_eq!(
            second.spans_redacted, 0,
            "placeholders must pass through untouched"
        );
        assert_eq!(body["messages"][0]["content"].as_str().unwrap(), once);
        unredact_check(&s, "sess", &once, input);
    }

    /// One value, one token, wherever it appears: the prompt's secret and
    /// the tool result's copy must redact identically, so the provider's
    /// prefix stays stable and restore needs a single entry.
    #[test]
    fn prompt_and_tool_result_share_placeholders() {
        let s = store();
        let secret = "sk-abcdefghij1234567890";
        let mut body = json!({"messages": [
            {"role": "user", "content": format!("my key is {secret}")},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Bash",
                 "input": {"command": format!("export K={secret}")}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1",
                 "content": format!("saved {secret} to /home/testuser/.env")}
            ]},
        ]});
        redact_body(&s, "sess", &mut body);
        let out = serde_json::to_string(&body).unwrap();
        assert!(!out.contains(secret), "raw secret leaked: {out}");
        let mut tokens = std::collections::HashSet::new();
        let mut j = 0;
        while let Some(k) = out[j..].find("__HR_SECRET_") {
            let start = j + k;
            // Read to the token's own terminator: a token carries a whole
            // ciphertext, so its length follows the value it replaced.
            let body_start = start + "__HR_SECRET_".len();
            let end = out[body_start..]
                .find("__")
                .map(|e| body_start + e + 2)
                .expect("minted token shape");
            tokens.insert(out[start..end].to_string());
            j = start + 1;
        }
        assert_eq!(tokens.len(), 1, "one value must mint one token: {tokens:?}");
        // And it restores everywhere, including inside tool_use input.
        unredact_body(&s, "sess", &mut body);
        let back = serde_json::to_string(&body).unwrap();
        assert_eq!(back.matches(secret).count(), 3, "got: {back}");
    }

    /// Restart simulation: a fresh store mints the identical token, so the
    /// provider's cached prefix survives a proxy restart with no recache —
    /// and the rebuilt map still restores.
    #[test]
    fn tokens_survive_a_store_restart() {
        let input = "key sk-abcdefghij1234567890 in /home/testuser/x/y.py";
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

    #[test]
    fn an_env_file_dump_leaves_no_value_in_the_clear() {
        let s = RedactStore::with_key([0xA5; 32]);
        // Fake values, real shapes.
        let text = "aws_access_key_id = AKIAQQQQWWWWEEEERRRR\n\
                aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n\
                capmonster_api_key = 0123456789abcdef0123456789abcdef\n\
                gmail_otp_app_password = abcd efgh ijkl mnop\n\
                proxy_url = http://user-abc:pass123@gw.example.com:823\n";
        let mut body = json!({"messages": [{"role": "user", "content": text}]});
        redact_body(&s, "sess", &mut body);
        let out = body["messages"][0]["content"].as_str().unwrap().to_string();
        let leaked: Vec<&str> = [
            "AKIAQQQQWWWWEEEERRRR",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "0123456789abcdef0123456789abcdef",
            "abcd efgh ijkl mnop",
            "pass123",
        ]
        .into_iter()
        .filter(|v| out.contains(v))
        .collect();
        assert!(leaked.is_empty(), "unmasked: {leaked:?}\n---\n{out}");
        unredact_check(&s, "sess", &out, text);
    }

    #[test]
    fn gate_disabled_passes_bytes_through() {
        let s = store();
        let gate = super::RedactGate::new(false, &s, "sess");
        let body = bytes::Bytes::from_static(b"{\"messages\":[{\"role\":\"user\"}]}");
        let (out, seam) = gate.seam_bytes(body.clone());
        assert!(seam.is_none());
        assert_eq!(out, body);
    }

    #[test]
    fn gate_passes_non_json_through() {
        let s = store();
        let gate = super::RedactGate::new(true, &s, "sess");
        let body = bytes::Bytes::from_static(b"not json at all");
        let (out, seam) = gate.seam_bytes(body.clone());
        assert!(seam.is_none());
        assert_eq!(out, body);
    }

    #[test]
    fn gate_passes_insensitive_json_through_untouched() {
        let s = store();
        let gate = super::RedactGate::new(true, &s, "sess");
        let body = bytes::Bytes::from_static(br#"{"model":"x","messages":[]}"#);
        let (out, seam) = gate.seam_bytes(body.clone());
        // Clean bodies keep the seam (the response may quote older tokens)
        // but the wire bytes stay identical.
        assert!(seam.is_some());
        assert_eq!(out, body);
    }

    #[test]
    fn gate_roundtrip_restores_redacted_bytes() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        let s = store();
        let gate = super::RedactGate::new(true, &s, "sess");
        let original = format!(
            "{{\"messages\":[{{\"role\":\"user\",\"content\":\"read {home}/src/main.rs\"}}]}}"
        );
        let (redacted, seam) = gate.seam_bytes(bytes::Bytes::from(original.clone()));
        // Scanner coverage of the ambient home varies by machine: when it
        // bites, the bytes must round-trip with no misses; when it does
        // not, the body must pass through byte-equal.
        match seam {
            Some(_) => {
                assert_ne!(redacted.as_ref(), original.as_bytes());
                let table = super::restore_table(&s, "sess").expect("table");
                let (restored, misses) = table.restore_bytes(&redacted);
                assert_eq!(misses, 0);
                assert_eq!(restored, original.as_bytes());
            }
            None => assert_eq!(redacted.as_ref(), original.as_bytes()),
        }
    }
}
