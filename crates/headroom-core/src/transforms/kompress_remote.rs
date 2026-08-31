//! Remote Kompress: offload ML compression to a hosted `/compress` endpoint.
//!
//! Rust port of `headroom/transforms/kompress_remote.py`. Lets a sandboxed
//! proxy built **without** the `ml` feature (no `ort`, no ONNX model download)
//! still run Kompress by calling a remote endpoint over HTTP. The public
//! surface mirrors [`Kompress`](super::kompress::Kompress) closely enough to be
//! a drop-in at the ContentRouter seam, and the type also implements the
//! name-addressable [`Compressor`] contract.
//!
//! Only the model inference is remote. The CCR store + retrieval marker stay
//! proxy-local — the endpoint is stateless (`enable_ccr = false` on its side),
//! so `headroom_retrieve` keeps working and original content never persists
//! off-box.
//!
//! # Transport is injected, not owned
//!
//! `headroom-core` has no HTTP client dependency (`hf-hub` vendors `ureq`
//! privately; nothing else in the crate speaks HTTP), and adding one for a
//! single POST is not this module's call to make. So the network hop is a
//! trait — [`KompressTransport`] — supplied by the embedding crate, which
//! already has a client. That also makes the module testable with no network.
//!
//! # Fail-open
//!
//! Any transport error, non-2xx status, or malformed response body returns the
//! content verbatim. A flaky endpoint degrades compression; it never breaks the
//! proxy request. This mirrors Python's blanket `except Exception` around the
//! whole request/parse block.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use super::compressor_registry::{CompressInput, CompressOutput, Compressor, CompressorDescriptor};
use super::kompress::{KompressResult, DEFAULT_MIN_WORDS, DEFAULT_MODEL_ID, MIN_WORDS};

/// Accept-any-shrink CCR gate, identical to Python's `KompressCompressor
/// .compress`: only store + mark when the shrink is worth the retrieval
/// marker's own cost.
const CCR_RATIO_GATE: f64 = 0.8;

/// Registered name. Deliberately the same as the in-process compressor's — the
/// remote client is a drop-in replacement, not a second entry in the registry.
const COMPRESSOR_NAME: &str = "kompress_compressor";

/// Appended to the configured endpoint unless [`RemoteKompressCompressor::with_path`]
/// says otherwise. An operator whose stack already serves a full path
/// (`/v1/models/kompress:predict`) passes `""` and gives the complete URL instead.
pub const DEFAULT_ENDPOINT_PATH: &str = "/compress";

/// Parse a `k=v,k2=v2` header string into ordered pairs.
///
/// Same format as `HEADROOM_OTEL_METRICS_HEADERS`, so operators meet one
/// convention. Blank items, items without `=`, and items with an empty key or
/// value are dropped rather than rejected — a malformed entry must not take the
/// endpoint down with it.
pub fn parse_endpoint_headers(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|item| {
            let (key, value) = item.trim().split_once('=')?;
            let (key, value) = (key.trim(), value.trim());
            (!key.is_empty() && !value.is_empty()).then(|| (key.to_string(), value.to_string()))
        })
        .collect()
}

// ─── Transport seam ─────────────────────────────────────────────────────

/// The single HTTP hop this module needs.
///
/// Implementors POST `body` (already-serialized JSON) to `url` and return the
/// response body. A non-2xx status must be reported as `Err` — that is the
/// counterpart of Python's `resp.raise_for_status()`, and it lands in the same
/// fail-open branch as a connection error.
pub trait KompressTransport: Send + Sync {
    /// POST `body` to `url` with `headers`, returning the response body.
    fn post(&self, url: &str, headers: &[(String, String)], body: &str) -> Result<String, String>;
}

// ─── Config ─────────────────────────────────────────────────────────────

/// Configuration for [`RemoteKompressCompressor`].
///
/// Only the two fields of Python's `KompressConfig` that the remote path
/// actually reads: everything else there (`device`, `chunk_words`,
/// `score_threshold`) is inference-side and lives on the endpoint.
#[derive(Debug, Clone)]
pub struct RemoteKompressConfig {
    /// Reported as `model_used` when the response omits the field.
    pub model_id: String,
    /// Store the original in the proxy-local CCR store and append a retrieval
    /// marker when the shrink clears [`CCR_RATIO_GATE`].
    pub enable_ccr: bool,
    /// Same floor contract as the in-process compressor: configurable, and
    /// clamped up to [`MIN_WORDS`] at the check.
    pub min_words: usize,
}

impl Default for RemoteKompressConfig {
    fn default() -> Self {
        Self {
            model_id: DEFAULT_MODEL_ID.to_string(),
            enable_ccr: true,
            min_words: DEFAULT_MIN_WORDS,
        }
    }
}

/// Persists `(original, compressed, original_tokens)` in the proxy-local CCR
/// store and returns the retrieval hash, or `None` when the write failed.
///
/// The counterpart of Python's module-level `store_kompress_in_ccr`. Injected
/// rather than called directly because this crate does not own the store — the
/// same split the other Rust compressors use (see
/// [`ConfigCompressor::compress`](super::config_compressor::ConfigCompressor::compress)).
pub type CcrStoreHook = Arc<dyn Fn(&str, &str, usize) -> Option<String> + Send + Sync>;

/// What actually happened on a [`RemoteKompressCompressor::compress_remote`]
/// call, alongside the result.
///
/// Python signals this only implicitly — a passthrough is a `KompressResult`
/// whose `compressed` happens to equal its `original`. That is ambiguous when
/// the endpoint legitimately returns the input unchanged, and the registry
/// contract needs an unambiguous `compressed` flag, so it is explicit here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOutcome {
    /// The endpoint answered. `Some(hash)` when the result cleared the CCR
    /// gate and was stored, so a retrieval marker is appended.
    Compressed(Option<String>),
    /// Input was below the configured floor; returned verbatim with no round trip.
    TooShort,
    /// Transport, status, or parse failure. Content returned verbatim.
    FailedOpen(String),
}

// ─── Compressor ─────────────────────────────────────────────────────────

/// Drop-in for [`Kompress`](super::kompress::Kompress) that POSTs to a hosted
/// `/compress` endpoint.
pub struct RemoteKompressCompressor {
    config: RemoteKompressConfig,
    /// Base URL as configured, kept so [`Self::with_path`] can re-resolve `url`.
    endpoint: String,
    url: String,
    headers: Vec<(String, String)>,
    transport: Arc<dyn KompressTransport>,
    store: Option<CcrStoreHook>,
    descriptor: CompressorDescriptor,
}

impl RemoteKompressCompressor {
    /// Build a client for `endpoint` (the base URL; [`DEFAULT_ENDPOINT_PATH`]
    /// is appended — see [`Self::with_path`] to override).
    ///
    /// `token`, when present, becomes an `authorization: Bearer …` header —
    /// wired from `HEADROOM_KOMPRESS_ENDPOINT_TOKEN` by the caller.
    pub fn new(
        endpoint: &str,
        token: Option<&str>,
        config: RemoteKompressConfig,
        transport: Arc<dyn KompressTransport>,
    ) -> Self {
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        if let Some(token) = token.filter(|t| !t.is_empty()) {
            headers.push(("authorization".to_string(), format!("Bearer {token}")));
        }
        Self {
            config,
            endpoint: endpoint.to_string(),
            url: resolve_url(endpoint, DEFAULT_ENDPOINT_PATH),
            headers,
            transport,
            store: None,
            descriptor: CompressorDescriptor {
                name: COMPRESSOR_NAME.to_string(),
                content_types: vec!["text/plain".to_string()],
                lossless: false,
                cost_tier: "remote".to_string(),
                recoverable: true,
            },
        }
    }

    /// Override the path appended to the endpoint; `""` uses the endpoint
    /// verbatim.
    ///
    /// Real inference servers do not serve at `/compress` — TorchServe uses
    /// `/predictions/<model>`, KServe `/v1/models/<name>:predict`, SageMaker
    /// `/invocations` — and appending to those 404s. Because remote Kompress
    /// fails open, that 404 is invisible: compression silently stops instead of
    /// erroring, so the only prior workaround was a reverse proxy that existed
    /// purely to rename a path.
    ///
    /// Not calling this keeps [`DEFAULT_ENDPOINT_PATH`], so an existing Modal
    /// deployment sees a byte-identical request.
    pub fn with_path(mut self, path: &str) -> Self {
        self.url = resolve_url(&self.endpoint, path);
        self
    }

    /// Merge extra request headers, replacing any already set under the same
    /// name (ASCII case-insensitively).
    ///
    /// Applied after the Bearer token on purpose: it lets an operator replace
    /// `authorization` with whatever their gateway wants (`x-api-key`, a signed
    /// header, a tenant id) without a separate auth-scheme setting.
    ///
    /// **Divergence:** Python does a plain `dict.update`, so its replacement is
    /// exact-key. Matching header names case-insensitively here avoids emitting
    /// both `authorization` and `Authorization` on the wire.
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        for (key, value) in headers {
            match self
                .headers
                .iter_mut()
                .find(|(k, _)| k.eq_ignore_ascii_case(&key))
            {
                Some(slot) => *slot = (key, value),
                None => self.headers.push((key, value)),
            }
        }
        self
    }

    /// Attach the proxy-local CCR store hook. Without it the retrieval marker
    /// is never emitted — nothing lossy is advertised as recoverable unless it
    /// really was stored.
    pub fn with_ccr_store(mut self, store: CcrStoreHook) -> Self {
        self.store = Some(store);
        self
    }

    /// Nothing to load locally, so the router can go straight to `compress`.
    pub fn is_ready(&self) -> bool {
        true
    }

    /// The backend label the router reports in telemetry.
    pub fn ready_backend(&self) -> Option<&'static str> {
        Some("remote")
    }

    /// No-op counterpart of Python's `preload` / `ensure_background_load`:
    /// there is no local model to warm.
    pub fn preload(&self) -> &'static str {
        "remote"
    }

    /// Read-only view of the active config.
    pub fn config(&self) -> &RemoteKompressConfig {
        &self.config
    }

    /// The resolved POST target. Worth logging where the router builds this:
    /// under the fail-open contract a mistyped path never surfaces as an
    /// error, only as compression quietly doing nothing.
    pub fn url(&self) -> &str {
        &self.url
    }

    fn passthrough(&self, content: &str, n_words: usize) -> KompressResult {
        KompressResult {
            compressed: content.to_string(),
            original: content.to_string(),
            original_tokens: n_words,
            compressed_tokens: n_words,
            compression_ratio: 1.0,
            model_used: self.config.model_id.clone(),
        }
    }

    /// Compress `content` through the remote endpoint.
    ///
    /// `target_ratio` is forwarded verbatim (`null` when `None`). Returns the
    /// content unchanged on a short input or on any failure, with the
    /// [`RemoteOutcome`] saying which.
    pub fn compress_remote(
        &self,
        content: &str,
        target_ratio: Option<f64>,
    ) -> (KompressResult, RemoteOutcome) {
        let n_words = content.split_whitespace().count();
        // Below this the in-process compressor passes through verbatim; mirror
        // it so we never pay a round-trip on a trivially small block. Dropping
        // words from a short block is a net loss anyway — the retrieval marker
        // alone is ~20 words, and short blocks are disproportionately
        // instruction-like, where the drops read as garbling.
        if n_words < self.config.min_words.max(MIN_WORDS) {
            return (self.passthrough(content, n_words), RemoteOutcome::TooShort);
        }

        let body = serde_json::json!({
            "content": content,
            "target_ratio": target_ratio,
        })
        .to_string();

        let mut result = match self
            .transport
            .post(&self.url, &self.headers, &body)
            .and_then(|raw| self.parse_response(&raw, content, n_words))
        {
            Ok(result) => result,
            Err(e) => {
                // Fail OPEN — never break the proxy on a bad endpoint.
                tracing::warn!(error = %e, "Remote Kompress failed; passing through");
                return (
                    self.passthrough(content, n_words),
                    RemoteOutcome::FailedOpen(e),
                );
            }
        };

        // CCR stays PROXY-LOCAL: the endpoint is stateless, so we store the
        // mapping + append the retrieval marker here — same policy and marker
        // format as the in-process compressor.
        if self.config.enable_ccr && result.compression_ratio < CCR_RATIO_GATE {
            if let Some(store) = &self.store {
                if let Some(cache_key) = store(content, &result.compressed, result.original_tokens)
                {
                    result.compressed.push_str(&ccr_marker(
                        result.original_tokens,
                        result.compressed_tokens,
                        content,
                        &cache_key,
                    ));
                    return (result, RemoteOutcome::Compressed(Some(cache_key)));
                }
            }
        }
        (result, RemoteOutcome::Compressed(None))
    }

    /// Parse a 200 response body into a [`KompressResult`].
    ///
    /// Every field is coerced *inside* the fail-open guard. A 200 with a
    /// malformed field (a non-numeric string, or an explicit JSON `null`, which
    /// Python's `float(None)` rejects) must degrade to passthrough rather than
    /// escape and break the request — that is the contract this class promises.
    fn parse_response(
        &self,
        raw: &str,
        content: &str,
        n_words: usize,
    ) -> Result<KompressResult, String> {
        let data: Value =
            serde_json::from_str(raw).map_err(|e| format!("response is not valid JSON: {e}"))?;
        let Some(obj) = data.as_object() else {
            return Err("remote Kompress response must be a JSON object".to_string());
        };
        let compressed = obj
            .get("compressed")
            .ok_or_else(|| "remote Kompress response is missing 'compressed'".to_string())?
            .as_str()
            .ok_or_else(|| {
                "remote Kompress response field 'compressed' must be a string".to_string()
            })?
            .to_string();

        let compressed_words = compressed.split_whitespace().count();
        Ok(KompressResult {
            original_tokens: coerce_int(obj.get("original_tokens"), n_words)?,
            compressed_tokens: coerce_int(obj.get("compressed_tokens"), compressed_words)?,
            compression_ratio: coerce_float(obj.get("compression_ratio"), 1.0)?,
            model_used: coerce_str(obj.get("model_used"), &self.config.model_id),
            compressed,
            original: content.to_string(),
        })
    }
}

/// Join `endpoint` and `path`. An empty `path` means the caller supplied a
/// complete URL and it is used verbatim.
fn resolve_url(endpoint: &str, path: &str) -> String {
    if path.is_empty() {
        return endpoint.to_string();
    }
    let endpoint = endpoint.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{endpoint}{path}")
    } else {
        format!("{endpoint}/{path}")
    }
}

/// The retrieval marker appended to a CCR-stored result. Byte-identical to the
/// Python reference's f-string, including the leading newline.
///
/// `source` is the pre-compression text: the marker reports its line span so a
/// reader can tell content was compressed away rather than absent. "items"
/// counts words, which does not map to lines and reads as evidence of absence
/// (#2586).
fn ccr_marker(
    original_tokens: usize,
    compressed_tokens: usize,
    source: &str,
    cache_key: &str,
) -> String {
    let source_lines = source.matches('\n').count() + 1;
    let line_word = if source_lines == 1 { "line" } else { "lines" };
    format!(
        "\n[{original_tokens} items compressed to \
         {compressed_tokens} (from {source_lines} source {line_word}). \
         Retrieve more: hash={cache_key}]"
    )
}

// ─── Field coercion (Python `int()` / `float()` / `str()` semantics) ─────

/// Python `int(...)` over a JSON value, with `default` for a missing key.
///
/// Accepts a JSON number (truncating toward zero, as `int(3.9) == 3`), a bool
/// (`int(True) == 1`), or a string of decimal digits. `null` and a non-integer
/// string are errors, matching `int(None)` / `int("3.5")` raising.
///
/// **Divergence:** Python would accept a negative count and build a
/// `KompressResult` with it; Rust's token fields are `usize`, so a negative
/// value is rejected into the fail-open branch instead. A negative token count
/// is malformed either way — Python just carries the corruption further.
fn coerce_int(value: Option<&Value>, default: usize) -> Result<usize, String> {
    let Some(value) = value else {
        return Ok(default);
    };
    let n = match value {
        Value::Bool(b) => i64::from(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i
            } else {
                let f = n
                    .as_f64()
                    .ok_or_else(|| "unrepresentable number".to_string())?;
                if !f.is_finite() {
                    return Err(format!("cannot int() a non-finite number: {f}"));
                }
                f.trunc() as i64
            }
        }
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("cannot int() the string {s:?}"))?,
        other => return Err(format!("cannot int() {other}")),
    };
    usize::try_from(n).map_err(|_| format!("negative token count: {n}"))
}

/// Python `float(...)` over a JSON value, with `default` for a missing key.
///
/// Accepts a JSON number, a bool, or a parseable numeric string (`"3.5"` is
/// fine here, unlike `int`). `null` is an error, matching `float(None)`.
fn coerce_float(value: Option<&Value>, default: f64) -> Result<f64, String> {
    let Some(value) = value else {
        return Ok(default);
    };
    match value {
        Value::Bool(b) => Ok(f64::from(*b)),
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| "unrepresentable number".to_string()),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("cannot float() the string {s:?}")),
        other => Err(format!("cannot float() {other}")),
    }
}

/// Python `str(...)` over a JSON value, with `default` for a missing key.
///
/// `str()` never raises, so neither does this. Scalars render the Python way
/// (`None`, `True`, `False`); a present-but-null `model_used` therefore becomes
/// the literal `"None"`, exactly as in the reference.
///
/// **Divergence:** an array or object renders as compact JSON rather than
/// Python's `repr` (`[1,2]` vs `[1, 2]`, `{"a":1}` vs `{'a': 1}`). Both are
/// garbage in a `model_used` field; neither affects compression.
fn coerce_str(value: Option<&Value>, default: &str) -> String {
    match value {
        None => default.to_string(),
        Some(Value::Null) => "None".to_string(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => "False".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Parse a `target_ratio` out of the pure-data config map.
///
/// [`CompressInput::config`] is `BTreeMap<String, String>`, so Python's
/// `float | None` arrives as text. An absent key means "no target ratio" (the
/// endpoint then uses its own threshold); an unparseable one is dropped with a
/// warning rather than failing the call, since the ratio is a hint.
fn target_ratio_from(config: &BTreeMap<String, String>) -> (Option<f64>, Option<String>) {
    let Some(raw) = config.get("target_ratio") else {
        return (None, None);
    };
    match raw.trim().parse::<f64>() {
        Ok(v) => (Some(v), None),
        Err(_) => (
            None,
            Some(format!("ignoring unparseable target_ratio {raw:?}")),
        ),
    }
}

impl Compressor for RemoteKompressCompressor {
    fn descriptor(&self) -> &CompressorDescriptor {
        &self.descriptor
    }

    fn compress(&self, input: &CompressInput) -> CompressOutput {
        let (target_ratio, ratio_warning) = target_ratio_from(&input.config);
        let (result, outcome) = self.compress_remote(&input.content, target_ratio);

        let mut warnings: Vec<String> = ratio_warning.into_iter().collect();
        let mut markers = Vec::new();
        let mut recoverable = BTreeMap::new();

        // The short-input gate and the fail-open branch both leave `content`
        // untouched, which is what `compressed: false` tells the caller.
        let passthrough = !matches!(outcome, RemoteOutcome::Compressed(_));
        match outcome {
            RemoteOutcome::FailedOpen(reason) => warnings.push(reason),
            RemoteOutcome::Compressed(Some(cache_key)) => {
                markers.push(ccr_marker(
                    result.original_tokens,
                    result.compressed_tokens,
                    &result.original,
                    &cache_key,
                ));
                recoverable.insert(cache_key, result.original.clone());
            }
            RemoteOutcome::Compressed(None) | RemoteOutcome::TooShort => {}
        }

        CompressOutput {
            content: result.compressed,
            tokens_before: result.original_tokens,
            tokens_after: result.compressed_tokens,
            lossless: false,
            markers,
            recoverable,
            warnings,
            compressed: !passthrough,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records the request and replays a canned response. No network.
    struct StubTransport {
        response: Result<String, String>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl StubTransport {
        fn ok(body: &str) -> Arc<Self> {
            Arc::new(Self {
                response: Ok(body.to_string()),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn err(message: &str) -> Arc<Self> {
            Arc::new(Self {
                response: Err(message.to_string()),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl KompressTransport for StubTransport {
        fn post(
            &self,
            url: &str,
            _headers: &[(String, String)],
            body: &str,
        ) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push((url.to_string(), body.to_string()));
            self.response.clone()
        }
    }

    /// 12 words — clears the hard 10-word clamp, not the 64-word default.
    const LONG: &str = "alpha bravo charlie delta echo foxtrot golf hotel india juliett kilo lima";

    /// The seams under test here are transport, metadata and CCR, none of
    /// which run on a block the floor turns away. Drop the floor to its clamp
    /// so a 12-word fixture still reaches them.
    fn test_config() -> RemoteKompressConfig {
        RemoteKompressConfig {
            min_words: MIN_WORDS,
            ..Default::default()
        }
    }

    fn client(transport: Arc<StubTransport>) -> RemoteKompressCompressor {
        RemoteKompressCompressor::new(
            "https://kompress.example.com",
            Some("s3cret"),
            test_config(),
            transport,
        )
    }

    #[test]
    fn endpoint_gets_a_compress_suffix_and_trailing_slashes_stripped() {
        let c = client(StubTransport::ok("{}"));
        assert_eq!(c.url(), "https://kompress.example.com/compress");

        let c = RemoteKompressCompressor::new(
            "https://kompress.example.com///",
            None,
            RemoteKompressConfig::default(),
            StubTransport::ok("{}"),
        );
        assert_eq!(c.url(), "https://kompress.example.com/compress");
    }

    #[test]
    fn a_path_override_retargets_the_endpoint() {
        // TorchServe / KServe style paths, which 404 under a hardcoded
        // /compress — and fail-open makes that 404 invisible.
        let c = client(StubTransport::ok("{}")).with_path("/predictions/kompress");
        assert_eq!(c.url(), "https://kompress.example.com/predictions/kompress");

        // A leading slash is optional.
        let c = client(StubTransport::ok("{}")).with_path("v1/models/kompress:predict");
        assert_eq!(
            c.url(),
            "https://kompress.example.com/v1/models/kompress:predict"
        );

        // Empty means the operator already gave a complete URL.
        let c = RemoteKompressCompressor::new(
            "https://ml.acme.com/invocations",
            None,
            RemoteKompressConfig::default(),
            StubTransport::ok("{}"),
        )
        .with_path("");
        assert_eq!(c.url(), "https://ml.acme.com/invocations");
    }

    #[test]
    fn extra_headers_are_applied_after_the_bearer_token() {
        // Applied last on purpose, so a gateway wanting x-api-key can drop
        // the Authorization header without a separate auth-scheme setting.
        let c = client(StubTransport::ok("{}")).with_headers(parse_endpoint_headers(
            "Authorization=Token abc, x-tenant-id=acme",
        ));
        assert!(c
            .headers
            .contains(&("Authorization".to_string(), "Token abc".to_string())));
        assert!(c
            .headers
            .contains(&("x-tenant-id".to_string(), "acme".to_string())));
        // Replaced, not duplicated — one authorization header on the wire.
        assert_eq!(
            c.headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .count(),
            1
        );
    }

    #[test]
    fn header_parsing_drops_malformed_items() {
        assert_eq!(parse_endpoint_headers(""), vec![]);
        assert_eq!(
            parse_endpoint_headers(" a=1 , junk, =2, b= , c=3 "),
            vec![
                ("a".to_string(), "1".to_string()),
                ("c".to_string(), "3".to_string())
            ]
        );
    }

    #[test]
    fn a_token_becomes_a_bearer_header_and_is_optional() {
        let c = client(StubTransport::ok("{}"));
        assert!(c
            .headers
            .contains(&("authorization".to_string(), "Bearer s3cret".to_string())));
        assert!(c
            .headers
            .contains(&("content-type".to_string(), "application/json".to_string())));

        let c = RemoteKompressCompressor::new(
            "https://x",
            None,
            RemoteKompressConfig::default(),
            StubTransport::ok("{}"),
        );
        assert_eq!(c.headers.len(), 1);
    }

    #[test]
    fn short_input_passes_through_without_a_round_trip() {
        let transport = StubTransport::ok(r#"{"compressed": "never used"}"#);
        let c = client(transport.clone());
        // 9 words, one below the clamp this config sits at.
        let short = "one two three four five six seven eight nine";
        let (r, outcome) = c.compress_remote(short, None);
        assert_eq!(transport.call_count(), 0, "must not call the endpoint");
        assert_eq!(outcome, RemoteOutcome::TooShort);
        assert_eq!(r.compressed, short);
        assert_eq!(r.original_tokens, 9);
        assert_eq!(r.compressed_tokens, 9);
        assert_eq!(r.compression_ratio, 1.0);
        assert_eq!(r.model_used, DEFAULT_MODEL_ID);
    }

    #[test]
    fn the_request_body_carries_content_and_target_ratio() {
        let transport = StubTransport::ok(r#"{"compressed": "alpha kilo"}"#);
        let c = client(transport.clone());
        c.compress_remote(LONG, Some(0.4));
        let body = transport.calls.lock().unwrap()[0].1.clone();
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(sent["content"], LONG);
        assert_eq!(sent["target_ratio"], 0.4);

        // `None` is sent as an explicit JSON null, as in Python.
        let transport = StubTransport::ok(r#"{"compressed": "alpha kilo"}"#);
        let c = client(transport.clone());
        c.compress_remote(LONG, None);
        let body = transport.calls.lock().unwrap()[0].1.clone();
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert!(sent["target_ratio"].is_null());
    }

    #[test]
    fn missing_metadata_falls_back_to_computed_defaults() {
        // Only `compressed` present: original_tokens = input words,
        // compressed_tokens = output words, ratio = 1.0, model = config's.
        let c = client(StubTransport::ok(
            r#"{"compressed": "alpha bravo charlie"}"#,
        ));
        let (r, _) = c.compress_remote(LONG, None);
        assert_eq!(r.compressed, "alpha bravo charlie");
        assert_eq!(r.original_tokens, 12);
        assert_eq!(r.compressed_tokens, 3);
        assert_eq!(r.compression_ratio, 1.0);
        assert_eq!(r.model_used, DEFAULT_MODEL_ID);
    }

    #[test]
    fn response_metadata_is_used_when_present() {
        let c = client(StubTransport::ok(
            r#"{"compressed": "alpha bravo", "original_tokens": 12,
                "compressed_tokens": 2, "compression_ratio": 0.9,
                "model_used": "remote-v3"}"#,
        ));
        let (r, outcome) = c.compress_remote(LONG, None);
        assert_eq!(r.original_tokens, 12);
        assert_eq!(r.compressed_tokens, 2);
        assert_eq!(r.compression_ratio, 0.9);
        assert_eq!(r.model_used, "remote-v3");
        // 0.9 is above the gate, so no CCR store and no marker.
        assert_eq!(outcome, RemoteOutcome::Compressed(None));
        assert!(!r.compressed.contains("Retrieve more"));
    }

    #[test]
    fn numeric_strings_are_coerced_like_python() {
        // Python: int("12") == 12, float("0.5") == 0.5.
        let c = client(StubTransport::ok(
            r#"{"compressed": "alpha bravo", "original_tokens": "12",
                "compressed_tokens": "2", "compression_ratio": "0.5",
                "model_used": 7}"#,
        ));
        let (r, _) = c.compress_remote(LONG, None);
        assert_eq!(r.original_tokens, 12);
        assert_eq!(r.compressed_tokens, 2);
        assert_eq!(r.compression_ratio, 0.5);
        // Python `str(7)` — the field is stringified, not rejected.
        assert_eq!(r.model_used, "7");
    }

    #[test]
    fn a_float_token_count_truncates_toward_zero() {
        // Python: int(3.9) == 3.
        let c = client(StubTransport::ok(
            r#"{"compressed": "alpha bravo", "compressed_tokens": 3.9}"#,
        ));
        let (r, _) = c.compress_remote(LONG, None);
        assert_eq!(r.compressed_tokens, 3);
    }

    #[test]
    fn transport_failure_fails_open() {
        let c = client(StubTransport::err("connection refused"));
        let (r, outcome) = c.compress_remote(LONG, None);
        assert_eq!(r.compressed, LONG, "content must survive a dead endpoint");
        assert_eq!(r.original_tokens, 12);
        assert_eq!(r.compressed_tokens, 12);
        assert_eq!(r.compression_ratio, 1.0);
        assert_eq!(
            outcome,
            RemoteOutcome::FailedOpen("connection refused".to_string())
        );
    }

    #[test]
    fn a_malformed_body_fails_open_rather_than_escaping() {
        // Each of these would raise inside Python's try block — TypeError on a
        // non-string `compressed`, float(None)/int(None) on an explicit null,
        // ValueError on a non-numeric string, JSONDecodeError on garbage.
        let bodies = [
            r#"{"compressed": 42}"#,
            r#"{"compressed": null}"#,
            r#"{"not_compressed": "x"}"#,
            r#"{"compressed": "alpha bravo", "compression_ratio": null}"#,
            r#"{"compressed": "alpha bravo", "original_tokens": null}"#,
            r#"{"compressed": "alpha bravo", "original_tokens": "not a number"}"#,
            r#"{"compressed": "alpha bravo", "original_tokens": "3.5"}"#,
            r#"["not", "an", "object"]"#,
            "not json at all",
        ];
        for body in bodies {
            let c = client(StubTransport::ok(body));
            let (r, outcome) = c.compress_remote(LONG, None);
            assert_eq!(r.compressed, LONG, "must pass through on body {body}");
            assert!(
                matches!(outcome, RemoteOutcome::FailedOpen(_)),
                "expected a fail-open reason for {body}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn a_null_model_used_stringifies_to_none_like_python() {
        // `str(None)` == "None": present-but-null does NOT fall back to the
        // config default, because `dict.get` returned the null, not the default.
        let c = client(StubTransport::ok(
            r#"{"compressed": "alpha bravo", "model_used": null}"#,
        ));
        let (r, _) = c.compress_remote(LONG, None);
        assert_eq!(r.model_used, "None");
    }

    #[test]
    fn a_shrink_below_the_gate_is_stored_and_marked() {
        let c = client(StubTransport::ok(
            r#"{"compressed": "alpha bravo", "original_tokens": 12,
                "compressed_tokens": 2, "compression_ratio": 0.17}"#,
        ))
        .with_ccr_store(Arc::new(|_, _, _| Some("abc123def456".to_string())));
        let (r, outcome) = c.compress_remote(LONG, None);
        assert_eq!(
            outcome,
            RemoteOutcome::Compressed(Some("abc123def456".to_string()))
        );
        // LONG is a single line, hence the singular "source line".
        assert_eq!(
            r.compressed,
            "alpha bravo\n[12 items compressed to 2 (from 1 source line). \
             Retrieve more: hash=abc123def456]"
        );
    }

    #[test]
    fn the_marker_reports_the_source_line_span() {
        let c = client(StubTransport::ok(
            r#"{"compressed": "alpha bravo", "original_tokens": 12,
                "compressed_tokens": 2, "compression_ratio": 0.17}"#,
        ))
        .with_ccr_store(Arc::new(|_, _, _| Some("abc123def456".to_string())));
        // 12 words over 3 lines: the marker must report lines, not words.
        let source = "alpha bravo charlie delta\necho foxtrot golf hotel\nindia juliett kilo lima";
        let (r, _) = c.compress_remote(source, None);
        assert!(
            r.compressed.ends_with(
                "[12 items compressed to 2 (from 3 source lines). \
                 Retrieve more: hash=abc123def456]"
            ),
            "{}",
            r.compressed
        );
    }

    #[test]
    fn the_gate_is_exclusive_at_exactly_zero_point_eight() {
        // Python: `result.compression_ratio < _CCR_RATIO_GATE`.
        let store: CcrStoreHook = Arc::new(|_, _, _| Some("deadbeef".to_string()));
        let at_gate = client(StubTransport::ok(
            r#"{"compressed": "alpha bravo", "compression_ratio": 0.8}"#,
        ))
        .with_ccr_store(store.clone());
        assert!(!at_gate
            .compress_remote(LONG, None)
            .0
            .compressed
            .contains("hash="));

        let below = client(StubTransport::ok(
            r#"{"compressed": "alpha bravo", "compression_ratio": 0.79}"#,
        ))
        .with_ccr_store(store);
        assert!(below
            .compress_remote(LONG, None)
            .0
            .compressed
            .contains("hash="));
    }

    #[test]
    fn no_marker_without_a_store_or_with_ccr_disabled() {
        let body = r#"{"compressed": "alpha bravo", "compression_ratio": 0.1}"#;

        // No store hook wired: nothing lossy may claim to be recoverable.
        let c = client(StubTransport::ok(body));
        assert!(!c.compress_remote(LONG, None).0.compressed.contains("hash="));

        // Store present but the write failed.
        let c = client(StubTransport::ok(body)).with_ccr_store(Arc::new(|_, _, _| None));
        assert!(!c.compress_remote(LONG, None).0.compressed.contains("hash="));

        // CCR disabled by config.
        let c = RemoteKompressCompressor::new(
            "https://x",
            None,
            RemoteKompressConfig {
                enable_ccr: false,
                ..test_config()
            },
            StubTransport::ok(body),
        )
        .with_ccr_store(Arc::new(|_, _, _| Some("abc".to_string())));
        assert!(!c.compress_remote(LONG, None).0.compressed.contains("hash="));
    }

    #[test]
    fn the_store_hook_sees_the_pre_marker_compressed_text() {
        let seen: Arc<Mutex<Vec<(String, String, usize)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let c = client(StubTransport::ok(
            r#"{"compressed": "alpha bravo", "original_tokens": 12,
                "compression_ratio": 0.1}"#,
        ))
        .with_ccr_store(Arc::new(move |orig, comp, tokens| {
            sink.lock()
                .unwrap()
                .push((orig.to_string(), comp.to_string(), tokens));
            Some("hash1".to_string())
        }));
        c.compress_remote(LONG, None);
        let calls = seen.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, LONG);
        assert_eq!(calls[0].1, "alpha bravo", "marker must not be stored");
        assert_eq!(calls[0].2, 12);
    }

    // ─── Registry contract ──────────────────────────────────────────────

    fn input(content: &str) -> CompressInput {
        CompressInput {
            content: content.to_string(),
            content_type: "text/plain".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_descriptor_declares_a_recoverable_remote_compressor() {
        let c = client(StubTransport::ok("{}"));
        let d = c.descriptor();
        assert_eq!(d.name, "kompress_compressor");
        assert_eq!(d.cost_tier, "remote");
        assert!(!d.lossless);
        assert!(d.recoverable);
    }

    #[test]
    fn registry_compress_reports_a_real_result() {
        let c = client(StubTransport::ok(
            r#"{"compressed": "alpha bravo", "original_tokens": 12,
                "compressed_tokens": 2, "compression_ratio": 0.17}"#,
        ))
        .with_ccr_store(Arc::new(|_, _, _| Some("abc123def456".to_string())));
        let out = c.compress(&input(LONG));
        assert!(out.compressed);
        assert_eq!(out.tokens_before, 12);
        assert_eq!(out.tokens_after, 2);
        assert!(!out.lossless);
        assert_eq!(out.markers.len(), 1);
        assert_eq!(
            out.recoverable.get("abc123def456").map(String::as_str),
            Some(LONG)
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn registry_compress_reports_a_passthrough_on_failure() {
        let c = client(StubTransport::err("boom"));
        let out = c.compress(&input(LONG));
        assert!(!out.compressed, "a fail-open result is a passthrough");
        assert_eq!(out.content, LONG);
        assert_eq!(out.warnings, vec!["boom".to_string()]);
        assert!(out.markers.is_empty());
        assert!(out.recoverable.is_empty());
    }

    #[test]
    fn target_ratio_is_read_from_the_string_config_map() {
        let transport = StubTransport::ok(r#"{"compressed": "alpha bravo"}"#);
        let c = client(transport.clone());
        let mut inp = input(LONG);
        inp.config
            .insert("target_ratio".to_string(), "0.25".to_string());
        c.compress(&inp);
        let sent: Value = serde_json::from_str(&transport.calls.lock().unwrap()[0].1).unwrap();
        assert_eq!(sent["target_ratio"], 0.25);
    }

    #[test]
    fn an_unparseable_target_ratio_warns_instead_of_failing() {
        let transport = StubTransport::ok(r#"{"compressed": "alpha bravo"}"#);
        let c = client(transport.clone());
        let mut inp = input(LONG);
        inp.config
            .insert("target_ratio".to_string(), "aggressive".to_string());
        let out = c.compress(&inp);
        assert_eq!(out.warnings.len(), 1);
        let sent: Value = serde_json::from_str(&transport.calls.lock().unwrap()[0].1).unwrap();
        assert!(
            sent["target_ratio"].is_null(),
            "bad hint is dropped, not sent"
        );
    }

    #[test]
    fn the_local_surface_reports_a_ready_remote_backend() {
        let c = client(StubTransport::ok("{}"));
        assert!(c.is_ready());
        assert_eq!(c.ready_backend(), Some("remote"));
        assert_eq!(c.preload(), "remote");
    }
    #[test]
    fn the_default_floor_matches_the_in_process_compressor() {
        assert_eq!(RemoteKompressConfig::default().min_words, DEFAULT_MIN_WORDS);
    }

    #[test]
    fn a_block_under_the_default_floor_never_leaves_the_process() {
        // 12 words: over the hard clamp, under the default floor. The default
        // config must turn it away without a round trip.
        let transport = StubTransport::ok(r#"{"compressed": "never used"}"#);
        let c = RemoteKompressCompressor::new(
            "https://x",
            None,
            RemoteKompressConfig::default(),
            transport.clone(),
        );
        let (r, outcome) = c.compress_remote(LONG, None);
        assert_eq!(transport.call_count(), 0);
        assert_eq!(outcome, RemoteOutcome::TooShort);
        assert_eq!(r.compressed, LONG);
    }

    #[test]
    fn a_floor_below_the_clamp_is_raised_to_it() {
        let transport = StubTransport::ok(r#"{"compressed": "never used"}"#);
        let c = RemoteKompressCompressor::new(
            "https://x",
            None,
            RemoteKompressConfig {
                min_words: 0,
                ..Default::default()
            },
            transport.clone(),
        );
        let (_, outcome) = c.compress_remote("one two three", None);
        assert_eq!(transport.call_count(), 0, "3 words is still below the clamp");
        assert_eq!(outcome, RemoteOutcome::TooShort);
    }
}
