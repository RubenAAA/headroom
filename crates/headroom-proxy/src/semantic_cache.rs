//! Semantic cache for the Headroom proxy.
//!
//! Simple semantic cache based on message content hash with LRU eviction.
//! Uses `tokio::sync::RwLock` for async-safe access.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use hmac::KeyInit;
use serde_json::Value;

/// Cache-key partition for callers that present no credential.
///
/// Upstream `ee6cfccc`: with no signed bytes the HMAC has nothing to
/// separate, so every credential-less request shares one partition.
/// `serde_json` renders the `&str` as `"anon"`.
pub const ANONYMOUS_PARTITION: &str = "anon";

/// Per-process HMAC key for [`compute_cache_partition`].
///
/// Two `Uuid::new_v4` (OS randomness, no new dependency): partitions are
/// process-local by design — a restarted proxy must not serve one caller's
/// cached completion to another caller's identical request anyway, and an
/// unguessable key keeps the partition ids from confirming credential
/// guesses (upstream `ee6cfccc`).
fn partition_hmac_key() -> [u8; 32] {
    use std::sync::OnceLock;
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    *KEY.get_or_init(|| {
        let a = uuid::Uuid::new_v4().into_bytes();
        let b = uuid::Uuid::new_v4().into_bytes();
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(&a);
        key[16..].copy_from_slice(&b);
        key
    })
}

/// Header-name components that mark credential material.
///
/// Port of the shared rule in upstream `ee6cfccc` (`identity.py`): names
/// are lower-cased, `_` folds to `-`, matching is on whole components, so
/// `X-Goog-Api-Key` is a credential while the numeric
/// `tool-search-deferred-tokens` tag is not. `account`, `organization`
/// and `project` select the billed account (`chatgpt-account-id`,
/// `openai-organization`, `openai-project`): two callers sharing a key but
/// billing different orgs must not share entries.
const CREDENTIAL_NAME_COMPONENTS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "credentials",
    "authorization",
    "apikey",
    "key",
    "bearer",
    "cookie",
    "signature",
    "account",
    "organization",
    "project",
];

/// True when an inbound header carries a credential.
fn is_credential_header(name: &str) -> bool {
    if name
        .strip_prefix("x-headroom-")
        .or_else(|| name.strip_prefix("X-Headroom-"))
        .is_some()
    {
        return false;
    }
    let lower = name.trim().to_ascii_lowercase();
    if lower == "idempotency-key" || lower == "x-idempotency-key" {
        return false;
    }
    match lower.as_str() {
        "authorization" | "proxy-authorization" | "cookie" | "x-api-key" => true,
        _ => lower
            .replace('_', "-")
            .split('-')
            .any(|part| CREDENTIAL_NAME_COMPONENTS.contains(&part)),
    }
}

/// Partition id separating cache entries by inbound credential.
///
/// Port of upstream `ee6cfccc` (`semantic_cache_key_policy.py`): the
/// semantic cache was keyed only by request content and upstream, so a
/// second caller sending the identical request with a different provider
/// key was served the first caller's completion. Every credential header
/// present on the request (name lower-cased, value trimmed, pairs sorted)
/// is folded into an HMAC-SHA256 under a per-process key; empty values are
/// skipped so a present-but-empty header cannot launder a partition.
/// Credential-less requests share [`ANONYMOUS_PARTITION`]. The Rust proxy
/// has no custom identity resolver (upstream OSS default returns `None`
/// for the principal), so headers alone are the partition input.
pub fn compute_cache_partition(headers: &HeaderMap) -> String {
    use hmac::Mac as _;
    let mut pairs: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| is_credential_header(name.as_str()))
        .filter_map(|(name, value)| {
            let trimmed = value.to_str().unwrap_or_default().trim().to_string();
            (!trimmed.is_empty()).then(|| (name.as_str().to_ascii_lowercase(), trimmed))
        })
        .collect();
    if pairs.is_empty() {
        return ANONYMOUS_PARTITION.to_string();
    }
    pairs.sort();
    let mut parts = Vec::with_capacity(pairs.len() * 2);
    for (name, value) in &pairs {
        parts.push(format!("h:{name}"));
        parts.push(value.clone());
    }
    let canonical = serde_json::to_string(&parts).unwrap_or_else(|_| "[]".to_string());
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&partition_hmac_key())
        .expect("HMAC takes a key of any length");
    mac.update(canonical.as_bytes());
    format!("p_{}", &hex::encode(mac.finalize().into_bytes())[..32])
}

/// Normalized key JSON: `{"partition":..,"model":..,"messages":[stripped..],extra..}`.
///
/// Byte-identical to the `to_string(key_parts Map)` form this
/// replaced, plus the leading `partition` member: insertion order is
/// `partition`, `model`, `messages`, then `extra` in
/// map order, with compact separators and `serde_json` escaping
/// throughout. `cache_control` members are dropped at any depth; the
/// marker comment from the old tree walker still applies — a moving
/// breakpoint must not change the key. The one exception is a key of a
/// JSON Schema `properties` object, where `cache_control` is a
/// user-defined property name, not a marker (upstream `b84c4c9f`).
fn normalized_key_json(
    partition: &str,
    messages: &[&Value],
    model: &str,
    extra: &[(&str, &Value)],
) -> Vec<u8> {
    use std::io::Write as _;
    fn write_stripped<W: std::io::Write>(
        w: &mut W,
        v: &Value,
        in_properties: bool,
    ) -> std::io::Result<()> {
        match v {
            Value::Object(map) => {
                write!(w, "{{")?;
                let mut first = true;
                for (k, child) in map.iter() {
                    if k.as_str() == "cache_control" && !in_properties {
                        continue;
                    }
                    if !first {
                        write!(w, ",")?;
                    }
                    first = false;
                    serde_json::to_writer(&mut *w, &k)?;
                    write!(w, ":")?;
                    write_stripped(w, child, k.as_str() == "properties")?;
                }
                write!(w, "}}")
            }
            Value::Array(arr) => {
                write!(w, "[")?;
                for (i, item) in arr.iter().enumerate() {
                    if i > 0 {
                        write!(w, ",")?;
                    }
                    write_stripped(w, item, false)?;
                }
                write!(w, "]")
            }
            // Scalars (incl. numbers via `arbitrary_precision`) go
            // through `serde_json` itself, so escaping and number
            // spelling match the old form exactly.
            scalar => serde_json::to_writer(w, scalar)
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::Other)),
        }
    }

    let mut out = Vec::with_capacity(1024);
    // Infallible in practice; on error the caller hashes whatever
    // prefix was written, which is still deterministic for the input.
    let _ = (|| -> std::io::Result<()> {
        // Upstream `ee6cfccc`: the partition separates entries by inbound
        // credential, so identical bodies under different provider keys
        // hash differently. Written first to mirror the upstream key order.
        write!(out, "{{\"partition\":")?;
        serde_json::to_writer(&mut out, &partition)?;
        write!(out, ",\"model\":")?;
        serde_json::to_writer(&mut out, &model)?;
        write!(out, ",\"messages\":")?;
        write!(out, "[")?;
        for (i, msg) in messages.iter().enumerate() {
            if i > 0 {
                write!(out, ",")?;
            }
            write_stripped(&mut out, msg, false)?;
        }
        write!(out, "]")?;
        for (k, v) in extra.iter().copied() {
            write!(out, ",")?;
            serde_json::to_writer(&mut out, &k)?;
            write!(out, ":")?;
            write_stripped(&mut out, v, false)?;
        }
        write!(out, "}}")
    })();
    out
}

/// Fields besides the turn array that change the answer, and so have to
/// change the key. `instructions` is the Responses API's system prompt —
/// the counterpart of Anthropic's `system`.
const KEY_EXTRA_FIELDS: &[&str] = &[
    "system",
    "instructions",
    "tools",
    "tool_choice",
    "temperature",
    "top_p",
    "top_k",
    "max_tokens",
    "stop",
];

/// The turn array and extra fields a cache key has to cover, or `None` when
/// the body carries no array we recognize as the turn and so cannot be keyed.
///
/// Anthropic Messages and Chat Completions both put the turn in `messages`;
/// the Responses API puts it in a top-level `input` and its system prompt in
/// `instructions`. Reading only `messages` and defaulting a miss to an empty
/// array made the key blind to the request: every Responses turn on one model
/// hashed alike, so a second turn was served the first turn's cached body.
/// Gemini's `contents` collided the same way. Returning `None` keeps a shape
/// we cannot key out of the cache instead of keying it on nothing — a missed
/// cache hit costs latency, a false hit returns someone else's answer.
///
/// `upstream` is the URL the request is forwarded to, keyed as
/// `upstream_base_url`: an `x-headroom-base-url` override picks a different
/// gateway, and two gateways answering the same body are not interchangeable
/// (upstream `dfdc7251`). Taking it here makes the lookup and the store key on
/// it alike.
///
/// Upstream keys the base URL alone; the query string is keyed here on
/// purpose. It can change the answer (Azure's `?api-version=`, Anthropic's
/// `?beta=true`), and a client sends the same one every turn, so the only
/// hits it costs are between requests that were not the same request.
///
/// Borrows: the old form cloned the whole turn array plus every extra
/// field (measured 481x on a 100-tool body) just to hand them to
/// `compute_key`, which only reads them. Both callers use the result
/// immediately, so references tied to `parsed` are sufficient.
#[allow(clippy::type_complexity)]
pub fn cache_key_inputs<'a>(
    parsed: &'a Value,
    upstream: &'a Value,
) -> Option<(Vec<&'a Value>, Vec<(&'a str, &'a Value)>)> {
    let turns: Vec<&Value> = match parsed.get("messages").or_else(|| parsed.get("input")) {
        Some(Value::Array(items)) => items.iter().collect(),
        // `input` may also be a bare string rather than an array of items.
        Some(s @ Value::String(_)) => vec![s],
        _ => return None,
    };

    let mut extra = vec![("upstream_base_url", upstream)];
    for key in KEY_EXTRA_FIELDS {
        if let Some(val) = parsed.get(*key) {
            extra.push((*key, val));
        }
    }
    Some((turns, extra))
}

/// A cached response entry.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    // `Bytes` (refcounted): a cache hit clones the entry, and the old
    // `Vec<u8>` memcpy'd up to MBs per hit (measured 246x on 200KB).
    // `len()` and `Body::from` behave identically.
    pub response_body: bytes::Bytes,
    pub response_headers: HashMap<String, String>,
    pub created_at: Instant,
    pub ttl: Duration,
    pub hit_count: u64,
}

/// Simple semantic cache based on message content hash.
///
/// Uses `tokio::sync::RwLock` for async-safe concurrent access.
pub struct SemanticCache {
    max_entries: usize,
    ttl: Duration,
    entries: std::sync::RwLock<LruCache>,
}

struct LruCache {
    // `lru::LruCache` replaces the old hand-rolled `Vec<String> order +
    // HashMap` (measured ~1.8x on 5k ops at cap 1000): hits were
    // `position + remove(pos)` O(n) memmoves and eviction was
    // `remove(0)` O(n); both are O(1) now. Eviction order is
    // identical (least-recently-used first, `get` refreshes recency),
    // and the capacity-eviction log line below is preserved verbatim.
    cache: lru::LruCache<String, CacheEntry>,
}

impl LruCache {
    fn new(max_entries: usize) -> Self {
        // `NonZeroUsize::MIN` (1) for a zero cap, matching the old
        // behavior where `max_entries == 0` still held one entry
        // (the `while len >= 0` loop broke immediately on the empty
        // order vec, then inserted).
        let cap = std::num::NonZeroUsize::new(max_entries).unwrap_or(std::num::NonZeroUsize::MIN);
        Self {
            cache: lru::LruCache::new(cap),
        }
    }

    fn get(&mut self, key: &str) -> Option<&CacheEntry> {
        // `lru::get` refreshes recency exactly like the old
        // move-to-end, and returns `None` when absent.
        self.cache.get(key)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut CacheEntry> {
        // Already MRU from the `get` above, so the recency refresh
        // here is a no-op, matching the old plain-map lookup.
        self.cache.get_mut(key)
    }

    fn pop(&mut self, key: &str) -> Option<CacheEntry> {
        self.cache.pop(key)
    }

    fn insert(&mut self, key: String, entry: CacheEntry, max_entries: usize) {
        // Refresh path: an existing key never evicts, just goes MRU —
        // `put` on a present key updates in place without evicting.
        if self.cache.contains(&key) {
            self.cache.put(key, entry);
            return;
        }

        // Evict oldest until under capacity, logging each exactly as
        // before (same event, same fields).
        while self.cache.len() >= max_entries {
            match self.cache.pop_lru() {
                Some((oldest, evicted)) => {
                    tracing::info!(
                        event = "semantic_cache_capacity_eviction",
                        key_hash = %oldest,
                        entries = self.cache.len(),
                        max_entries,
                        entry_age_ms = evicted.created_at.elapsed().as_millis() as u64,
                        "semantic cache evicted an entry at capacity"
                    );
                }
                None => break,
            }
        }

        self.cache.put(key, entry);
    }

    fn len(&self) -> usize {
        self.cache.len()
    }

    fn total_hits(&self) -> u64 {
        self.cache.iter().map(|(_, e)| e.hit_count).sum()
    }
}

impl SemanticCache {
    pub fn new(max_entries: usize, ttl_seconds: u64) -> Self {
        Self {
            max_entries,
            ttl: Duration::from_secs(ttl_seconds),
            entries: std::sync::RwLock::new(LruCache::new(max_entries)),
        }
    }

    /// Compute cache key from partition, messages, model, and extra fields.
    ///
    /// The normalized key JSON is byte-identical to the old
    /// `to_string(key_parts Map)` form (pinned by
    /// `normalized_key_bytes_are_stable`) plus a leading `partition`
    /// member: same key order (`partition`, `model`, `messages`, then
    /// `extra` in field order), same compact separators, same
    /// `serde_json` escaping. Only the construction changed — the old
    /// path deep-cloned the whole turn array (`messages.to_vec()`) plus
    /// a stripped rebuild plus the map; this streams the stripped form
    /// straight into one buffer. `DefaultHasher` keys are process-local,
    /// so keys were never stable across deploys; within a process equal
    /// inputs still hash equal and marker moves still hash equal.
    ///
    /// The partition (see [`compute_cache_partition`], upstream `ee6cfccc`)
    /// separates entries by inbound credential: identical bodies under
    /// different provider keys hash differently, so one caller's cached
    /// completion is never served to another caller.
    fn compute_key(
        partition: &str,
        messages: &[&Value],
        model: &str,
        extra: &[(&str, &Value)],
    ) -> String {
        let normalized = normalized_key_json(partition, messages, model, extra);
        // `to_vec` output is valid UTF-8 by construction; `str::hash`
        // (bytes + 0xff terminator) is exactly what the old
        // `String::hash` fed the hasher, so key values are unchanged.
        let text = std::str::from_utf8(&normalized).unwrap_or_default();
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    /// Get cached response if exists and not expired.
    ///
    /// `partition` is [`compute_cache_partition`] output for the inbound
    /// request's headers (upstream `ee6cfccc`).
    pub fn get(
        &self,
        partition: &str,
        messages: &[&Value],
        model: &str,
        extra: &[(&str, &Value)],
    ) -> Option<CacheEntry> {
        let key = Self::compute_key(partition, messages, model, extra);
        let mut cache = self.entries.write().unwrap();

        let entry = match cache.get(&key) {
            Some(e) => e.clone(),
            None => {
                tracing::debug!(event = "semantic_cache_miss", key_hash = %key, "semantic cache miss");
                return None;
            }
        };

        // Check expiration
        if entry.created_at.elapsed() > entry.ttl {
            cache.pop(&key);
            tracing::info!(
                event = "semantic_cache_ttl_expiry",
                key_hash = %key,
                ttl_seconds = entry.ttl.as_secs(),
                age_ms = entry.created_at.elapsed().as_millis() as u64,
                "semantic cache entry expired"
            );
            return None;
        }

        // Increment hit count
        if let Some(e) = cache.get_mut(&key) {
            e.hit_count += 1;
        }
        tracing::debug!(event = "semantic_cache_hit", key_hash = %key, hit_count = entry.hit_count + 1, "semantic cache hit");

        Some(entry)
    }

    /// Cache a response.
    ///
    /// `partition` is [`compute_cache_partition`] output for the inbound
    /// request's headers (upstream `ee6cfccc`); lookup and store must use
    /// the partition computed from the same headers or hits never land.
    /// Eight args: the partition key is a separate security input that must
    /// not fold into `extra`.
    #[allow(clippy::too_many_arguments)]
    pub fn set(
        &self,
        partition: &str,
        messages: &[&Value],
        model: &str,
        response_body: impl Into<bytes::Bytes>,
        response_headers: HashMap<String, String>,
        _tokens_saved: u64,
        extra: &[(&str, &Value)],
    ) {
        let key = Self::compute_key(partition, messages, model, extra);
        let entry = CacheEntry {
            response_body: response_body.into(),
            response_headers,
            created_at: Instant::now(),
            ttl: self.ttl,
            hit_count: 0,
        };
        if !is_cacheable_reply(&entry.response_body) {
            return;
        }
        let mut cache = self.entries.write().unwrap();
        cache.insert(key, entry, self.max_entries);
    }

    /// Clear all cache entries.
    pub fn clear(&self) {
        let mut cache = self.entries.write().unwrap();
        cache.cache.clear();
    }

    /// Get cache statistics.
    pub fn stats(&self) -> HashMap<String, Value> {
        let cache = self.entries.read().unwrap();
        let mut stats = HashMap::new();
        stats.insert("entries".to_string(), serde_json::json!(cache.len()));
        stats.insert(
            "max_entries".to_string(),
            serde_json::json!(self.max_entries),
        );
        stats.insert(
            "total_hits".to_string(),
            serde_json::json!(cache.total_hits()),
        );
        stats.insert(
            "ttl_seconds".to_string(),
            serde_json::json!(self.ttl.as_secs()),
        );
        stats
    }
}

/// Whether a response body may be cached. Error replies delivered as 200
/// (or byte-identical error envelopes) must not replay: the next client
/// would receive the error as if it were the answer. Upstream `1132a54b`.
/// Non-JSON bytes (and non-JSON-shaped bodies, per upstream, since both
/// handlers only store bodies they already decoded) are stored as before.
fn is_cacheable_reply(response_body: &[u8]) -> bool {
    if response_body.is_empty() {
        return false;
    }
    match serde_json::from_slice::<Value>(response_body) {
        Ok(Value::Object(map)) => {
            if map.is_empty() {
                return false;
            }
            if map.get("type").and_then(Value::as_str) == Some("error") {
                return false;
            }
            match map.get("error") {
                Some(Value::Null) | None => true,
                Some(_) => false,
            }
        }
        Ok(_) => false,
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs() -> Vec<Value> {
        vec![serde_json::json!({"role": "user", "content": "hello"})]
    }

    /// Borrowed view for the `&[&Value]` key/cache APIs.
    fn refs(v: &[Value]) -> Vec<&Value> {
        v.iter().collect()
    }

    #[test]
    fn compute_key_same_for_same_input() {
        let extra: Vec<(&str, &Value)> = Vec::new();
        let k1 =
            SemanticCache::compute_key(ANONYMOUS_PARTITION, &refs(&msgs()), "claude-3", &extra);
        let k2 =
            SemanticCache::compute_key(ANONYMOUS_PARTITION, &refs(&msgs()), "claude-3", &extra);
        assert_eq!(k1, k2);
    }

    #[test]
    fn compute_key_differs_for_different_model() {
        let extra: Vec<(&str, &Value)> = Vec::new();
        let k1 =
            SemanticCache::compute_key(ANONYMOUS_PARTITION, &refs(&msgs()), "claude-3", &extra);
        let k2 =
            SemanticCache::compute_key(ANONYMOUS_PARTITION, &refs(&msgs()), "claude-4", &extra);
        assert_ne!(k1, k2);
    }

    /// The streaming writer must emit byte-identical normalized JSON to
    /// the `Map`-build + `to_string` form it replaced. Key parsing is
    /// deterministic (`serde_json` output), unlike `DefaultHasher`
    /// values (process-local), so this pins cross-version stability:
    /// a drift here cold-misses the whole cache on deploy at best, and
    /// risks false hits at worst.
    #[test]
    fn normalized_key_bytes_are_stable() {
        let extra: Vec<(&str, &Value)> = Vec::new();
        let bytes = normalized_key_json(ANONYMOUS_PARTITION, &refs(&msgs()), "claude-3", &extra);
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"partition":"anon","model":"claude-3","messages":[{"role":"user","content":"hello"}]}"#
        );
        // Markers stripped, key order and escaping preserved.
        let marked = vec![serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "hi \"yo\"", "cache_control": {"type": "ephemeral"}}],
        })];
        let bytes = normalized_key_json(ANONYMOUS_PARTITION, &refs(&marked), "m", &extra);
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"partition":"anon","model":"m","messages":[{"role":"user","content":[{"type":"text","text":"hi \"yo\""}]}]}"#
        );
    }

    #[test]
    fn error_shaped_bodies_are_not_cacheable() {
        assert!(!is_cacheable_reply(b""));
        assert!(!is_cacheable_reply(b"{}"));
        assert!(!is_cacheable_reply(
            br#"{"type":"error","error":{"type":"api_error"}}"#
        ));
        assert!(!is_cacheable_reply(br#"{"error":"overloaded"}"#));
        assert!(!is_cacheable_reply(br#"[1,2,3]"#));
        assert!(is_cacheable_reply(br#"{"error":null}"#));
        assert!(is_cacheable_reply(br#"{"data":"ok"}"#));
        assert!(is_cacheable_reply(b"\xff\xfe not json"));
    }

    #[test]
    fn set_skips_caching_error_replies() {
        let cache = SemanticCache::new(60, 8);
        let msgs = [serde_json::json!({"role": "user", "content": "hello"})];
        cache.set(
            ANONYMOUS_PARTITION,
            &[&msgs[0]],
            "m",
            serde_json::json!({"type":"error"}).to_string(),
            HashMap::new(),
            0,
            &[],
        );
        assert!(
            cache
                .get(ANONYMOUS_PARTITION, &[&msgs[0]], "m", &[])
                .is_none()
        );
        cache.set(
            ANONYMOUS_PARTITION,
            &[&msgs[0]],
            "m",
            "ok-body",
            HashMap::new(),
            0,
            &[],
        );
        assert!(
            cache
                .get(ANONYMOUS_PARTITION, &[&msgs[0]], "m", &[])
                .is_some()
        );
    }

    #[test]
    fn get_miss_on_empty_cache() {
        let cache = SemanticCache::new(100, 3600);
        let extra: Vec<(&str, &Value)> = Vec::new();
        assert!(
            cache
                .get(ANONYMOUS_PARTITION, &refs(&msgs()), "model", &extra)
                .is_none()
        );
    }

    /// Upstream `ee6cfccc`: two callers whose requests differ only in
    /// credential must not share a cache entry. Same body, same model,
    /// different `x-api-key` → store under one partition, miss under the
    /// other.
    #[test]
    fn different_credentials_do_not_share_entries() {
        let cache = SemanticCache::new(100, 3600);
        let extra: Vec<(&str, &Value)> = Vec::new();
        let headers = |key: &str| {
            let mut h = HeaderMap::new();
            h.insert("x-api-key", axum::http::HeaderValue::from_str(key).unwrap());
            h
        };
        let pa = compute_cache_partition(&headers("sk-caller-a"));
        let pb = compute_cache_partition(&headers("sk-caller-b"));
        assert_ne!(pa, pb);
        cache.set(
            &pa,
            &refs(&msgs()),
            "model",
            b"response-a".to_vec(),
            HashMap::new(),
            0,
            &extra,
        );
        assert!(cache.get(&pb, &refs(&msgs()), "model", &extra).is_none());
        let hit = cache.get(&pa, &refs(&msgs()), "model", &extra);
        assert_eq!(
            hit.unwrap().response_body,
            bytes::Bytes::from_static(b"response-a")
        );
    }

    /// Requests without any credential share the anonymous partition, so
    /// the single-tenant localhost path keeps its hit rate.
    #[test]
    fn credential_less_requests_share_the_anonymous_partition() {
        assert_eq!(
            compute_cache_partition(&HeaderMap::new()),
            ANONYMOUS_PARTITION
        );
        let mut plain = HeaderMap::new();
        plain.insert("content-type", "application/json".parse().unwrap());
        assert_eq!(compute_cache_partition(&plain), ANONYMOUS_PARTITION);
    }

    /// Non-credential headers (model routing, session tags, idempotency
    /// keys) must not move a request out of its credential partition, or
    /// retries and multi-turn sessions would cold-miss.
    #[test]
    fn non_credential_headers_do_not_move_partitions() {
        let headers = |extra: Option<(String, String)>| {
            let mut h = HeaderMap::new();
            h.insert("x-api-key", "sk-same".parse().unwrap());
            if let Some((k, v)) = extra {
                h.insert(
                    axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    v.parse().unwrap(),
                );
            }
            h
        };
        let base = compute_cache_partition(&headers(None));
        assert_ne!(base, ANONYMOUS_PARTITION);
        for extra in [
            Some(("x-headroom-session-id".to_string(), "s-1".to_string())),
            Some(("anthropic-model".to_string(), "claude-opus-5".to_string())),
            Some(("idempotency-key".to_string(), "retry-1".to_string())),
            Some(("x-idempotency-key".to_string(), "retry-2".to_string())),
        ] {
            assert_eq!(compute_cache_partition(&headers(extra)), base);
        }
    }

    /// One key billing different accounts, orgs or projects must not share
    /// entries: the account-selector headers move the partition.
    #[test]
    fn account_selector_headers_move_partitions() {
        let with = |name: &str, value: &str| {
            let mut h = HeaderMap::new();
            h.insert("authorization", "Bearer sk-same".parse().unwrap());
            h.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
            compute_cache_partition(&h)
        };
        for name in [
            "chatgpt-account-id",
            "openai-organization",
            "openai-project",
        ] {
            assert_ne!(with(name, "a"), with(name, "b"), "{name}");
        }
    }

    #[test]
    fn set_and_get_hit() {
        let cache = SemanticCache::new(100, 3600);
        let extra: Vec<(&str, &Value)> = Vec::new();
        cache.set(
            ANONYMOUS_PARTITION,
            &refs(&msgs()),
            "model",
            b"response".to_vec(),
            HashMap::new(),
            10,
            &extra,
        );
        let entry = cache.get(ANONYMOUS_PARTITION, &refs(&msgs()), "model", &extra);
        assert!(entry.is_some());
        assert_eq!(
            entry.unwrap().response_body,
            bytes::Bytes::from_static(b"response")
        );
    }

    #[test]
    fn lru_eviction() {
        let cache = SemanticCache::new(2, 3600);
        let extra: Vec<(&str, &Value)> = Vec::new();
        let m1 = vec![serde_json::json!({"role": "user", "content": "a"})];
        let m2 = vec![serde_json::json!({"role": "user", "content": "b"})];
        let m3 = vec![serde_json::json!({"role": "user", "content": "c"})];

        cache.set(
            ANONYMOUS_PARTITION,
            &refs(&m1),
            "m",
            b"one".to_vec(),
            HashMap::new(),
            0,
            &extra,
        );
        cache.set(
            ANONYMOUS_PARTITION,
            &refs(&m2),
            "m",
            b"two".to_vec(),
            HashMap::new(),
            0,
            &extra,
        );
        cache.set(
            ANONYMOUS_PARTITION,
            &refs(&m3),
            "m",
            b"three".to_vec(),
            HashMap::new(),
            0,
            &extra,
        );

        // m1 should be evicted
        assert!(
            cache
                .get(ANONYMOUS_PARTITION, &refs(&m1), "m", &extra)
                .is_none()
        );
        assert!(
            cache
                .get(ANONYMOUS_PARTITION, &refs(&m2), "m", &extra)
                .is_some()
        );
        assert!(
            cache
                .get(ANONYMOUS_PARTITION, &refs(&m3), "m", &extra)
                .is_some()
        );
    }

    #[test]
    fn stats_returns_correct_counts() {
        let cache = SemanticCache::new(100, 3600);
        let extra: Vec<(&str, &Value)> = Vec::new();
        cache.set(
            ANONYMOUS_PARTITION,
            &refs(&msgs()),
            "m",
            b"data".to_vec(),
            HashMap::new(),
            5,
            &extra,
        );
        let stats = cache.stats();
        assert_eq!(stats["entries"], serde_json::json!(1));
        assert_eq!(stats["max_entries"], serde_json::json!(100));
    }

    #[test]
    fn clear_empties_cache() {
        let cache = SemanticCache::new(100, 3600);
        let extra: Vec<(&str, &Value)> = Vec::new();
        cache.set(
            ANONYMOUS_PARTITION,
            &refs(&msgs()),
            "m",
            b"data".to_vec(),
            HashMap::new(),
            0,
            &extra,
        );
        cache.clear();
        assert!(
            cache
                .get(ANONYMOUS_PARTITION, &refs(&msgs()), "m", &extra)
                .is_none()
        );
        assert_eq!(cache.stats()["entries"], serde_json::json!(0));
    }
}

#[cfg(test)]
mod cache_key_marker_tests {
    use super::*;

    fn msgs(with_marker: bool) -> Vec<Value> {
        let mut block = serde_json::json!({"type": "text", "text": "hi"});
        if with_marker {
            block["cache_control"] = serde_json::json!({"type": "ephemeral"});
        }
        vec![serde_json::json!({"role": "user", "content": [block]})]
    }

    /// Borrowed view for the `&[&Value]` key API.
    fn refs(v: &[Value]) -> Vec<&Value> {
        v.iter().collect()
    }

    #[test]
    fn a_moved_breakpoint_does_not_fragment_the_key() {
        let extra: Vec<(&str, &Value)> = Vec::new();
        let with = SemanticCache::compute_key(
            ANONYMOUS_PARTITION,
            &refs(&msgs(true)),
            "claude-opus-5",
            &extra,
        );
        let without = SemanticCache::compute_key(
            ANONYMOUS_PARTITION,
            &refs(&msgs(false)),
            "claude-opus-5",
            &extra,
        );
        assert_eq!(
            with, without,
            "a cache_control marker in messages must not change the key"
        );
    }

    #[test]
    fn real_content_still_changes_the_key() {
        let extra: Vec<(&str, &Value)> = Vec::new();
        let a = SemanticCache::compute_key(
            ANONYMOUS_PARTITION,
            &refs(&msgs(false)),
            "claude-opus-5",
            &extra,
        );
        let b = SemanticCache::compute_key(
            ANONYMOUS_PARTITION,
            &refs(&[
                serde_json::json!({"role": "user", "content": [{"type": "text", "text": "bye"}]}),
            ]),
            "claude-opus-5",
            &extra,
        );
        assert_ne!(a, b, "different turn text must change the key");
    }

    /// `cache_control` as a tool-schema property name is content: two tool
    /// lists that differ only by it must not share a cached response. A real
    /// marker on the property's own schema is still dropped.
    #[test]
    fn a_schema_property_named_cache_control_changes_the_key() {
        let schema = |props: Value| {
            serde_json::json!([{"name": "maintain",
                "input_schema": {"type": "object", "properties": props}}])
        };
        let without = schema(serde_json::json!({}));
        let with = schema(serde_json::json!({"cache_control": {"type": "boolean"}}));
        let marked = schema(serde_json::json!({"cache_control": {
            "type": "boolean", "cache_control": {"type": "ephemeral"}}}));
        let key = |tools: &Value| {
            SemanticCache::compute_key(
                ANONYMOUS_PARTITION,
                &refs(&msgs(false)),
                "claude-opus-5",
                &[("tools", tools)],
            )
        };
        assert_ne!(key(&without), key(&with));
        assert_eq!(key(&with), key(&marked));
    }
}
