//! Kompress — Rust port of `headroom.transforms.kompress_compressor`.
//!
//! A ModernBERT token compressor for prose / plain-text tool outputs.
//! Where SmartCrusher/Log/Search/Diff are deterministic structural
//! compressors, Kompress is an **ML** compressor: it runs the trained
//! `chopratejas/kompress-v2-base` model (a fine-tune of
//! `answerdotai/ModernBERT-base` with a token keep/discard head + a
//! span-importance CNN head, exported to ONNX) and keeps only the words
//! the model scores as salient.
//!
//! # Model layering
//!
//! - **Inference weights:** `chopratejas/kompress-v2-base` — the ONNX
//!   artifact (`onnx/kompress-int8-wo.onnx`, weight-only int8 via the
//!   `com.microsoft` `MatMulNBits` contrib op; falls through to
//!   `onnx/kompress-fp32.onnx` then `onnx/kompress-int8.onnx`). This is
//!   *the model behind text compression*.
//! - **Tokenizer:** `answerdotai/ModernBERT-base`'s `tokenizer.json`.
//!   Kompress is a fine-tune of ModernBERT and reuses its exact vocab,
//!   so the kompress repo ships no tokenizer of its own.
//!
//! # ONNX contract
//!
//! Inputs `input_ids` + `attention_mask` (both `int64`, shape
//! `[batch, seq]`); output `final_scores` (`f32`, shape `[batch, seq]`)
//! — per-token salience in `[0, 1]` with the dual-head logic baked into
//! the graph. Keep decision is `score > 0.5`.
//!
//! # Compression path (mirrors the Python ONNX/proxy path exactly)
//!
//! 1. `words = content.split_whitespace()`. If `< 10` words → passthrough.
//! 2. For each `chunk_words`-sized (default 350) window of words:
//!    tokenize with the word list as **pre-tokenized** input
//!    (`is_split_into_words=True` in `transformers`), truncating to 512
//!    tokens; recover `input_ids` / `attention_mask` / `word_ids`.
//! 3. Run ONNX → `final_scores`. Reduce to **max score per word**.
//! 4. Keep word `w` (global index `w + chunk_start`) when its max score
//!    exceeds the threshold (default 0.5), or, when `target_ratio` is
//!    set, when it is in the top-`ratio` fraction by score.
//! 5. Emit the kept words, in original order, joined by single spaces.
//!
//! # Parity
//!
//! Byte-exact against the Python reference on the ONNX path: tokenizer
//! `input_ids`/`word_ids` reproduce `transformers` exactly, ONNX scores
//! match to ~1e-6 (far below the 0.5 threshold), and the kept-word set +
//! joined output match byte-for-byte. See
//! `tests/parity/fixtures/kompress/` and `KompressComparator` in
//! `crates/headroom-parity`.
//!
//! # CCR
//!
//! This engine returns the compressed string only. CCR offload of the
//! dropped words (so the model can retrieve the original on demand) is
//! handled by the live-zone dispatcher via [`crate::ccr::CcrStore`],
//! exactly as for the Search/Log/Diff compressors — not inside this
//! engine. The Python reference's inline `[N items compressed... hash=]`
//! marker is intentionally **not** reproduced; the Rust side uses the
//! canonical `<<ccr:HASH>>` marker convention.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ort::session::Session;
use ort::value::Tensor;
use regex::Regex;
use thiserror::Error;
use tokenizers::tokenizer::TruncationParams;
use tokenizers::{EncodeInput, InputSequence, Tokenizer};

// ─── Tunable defaults (parity-pinned to kompress-v2-base) ───────────────

/// HuggingFace repo holding the trained ONNX weights.
pub const DEFAULT_MODEL_ID: &str = "chopratejas/kompress-v2-base";
/// HuggingFace repo holding the tokenizer Kompress reuses.
pub const DEFAULT_TOKENIZER_REPO: &str = "answerdotai/ModernBERT-base";
/// Words per inference chunk. Coupled to the model's training window;
/// kompress-v2-base was trained for 350.
pub const DEFAULT_CHUNK_WORDS: usize = 350;
/// Keep a word when its max per-token score exceeds this. Matches the
/// ONNX `get_keep_mask` hard-coded `> 0.5`.
pub const DEFAULT_SCORE_THRESHOLD: f32 = 0.5;
/// Hard floor. Inputs shorter than this many words pass through
/// untouched — too little signal for the model and the per-call cost
/// dominates. [`KompressConfig::min_words`] is clamped up to it.
pub const MIN_WORDS: usize = 10;
/// Default for [`KompressConfig::min_words`]. Word-dropping below this
/// size is a net loss: the CCR retrieval marker alone is ~20 words, and
/// short blocks are disproportionately instruction-like (sanitizer
/// banners, section headers) where dropped words read as garbling rather
/// than compression. Mirrors Python `KompressConfig.min_input_words`.
pub const DEFAULT_MIN_WORDS: usize = 64;
/// Max ModernBERT sequence length per chunk (truncation bound).
pub const MAX_SEQ_LEN: usize = 512;

// ─── Must-keep regex (mirrors Python `_KOMPRESS_MUST_KEEP_RE`) ──────────
//
// Tokens matching this pattern are always kept regardless of model score.
// Numbers, ALLCAPS identifiers, dotted paths, unix paths, file extensions,
// CLI flags, and CamelCase names carry semantic meaning that agents cannot
// reconstruct from context — dropping them degrades reasoning correctness.
// Disable with HEADROOM_KOMPRESS_MUST_KEEP=0.
//
// NOTE: Python's regex uses look-around (`(?<![\w.])` / `(?![\w.])`) for
// standalone numbers, but Rust's `regex` crate doesn't support look-around.
// We use `\b` word boundaries instead, which is slightly less precise but
// covers the same practical cases (numbers surrounded by whitespace/punctuation).
fn must_keep_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?x)
            \b0x[0-9A-Fa-f]+\b                        # hex addresses/IDs: 0x7fff2038
            | \b\d+(?:\.\d+)?\b                        # standalone numbers: 42, 3.14
            | [A-Z_]{2,}                                # ALLCAPS: SIGILL, HTTP, EOF, ERROR
            | [a-z_][a-z0-9_]*\.[a-z0-9_]+             # dotted.paths: libsystem_kernel.dylib
            | /[a-z0-9/._-]{2,}                         # unix paths: /usr/lib/python3.so
            | \.[a-z]{2,4}\b                            # extensions: .py .so .json
            | --?[a-z][\w-]*                            # flags: --verbose, -n
            | \b[A-Z][a-z]+[A-Z]\w*                    # CamelCase: EXC_BAD_INSTRUCTION, IndexError
            ",
        )
        .expect("MUST_KEEP_RE is a valid regex")
    })
}

/// Env var to disable the must-keep safety net (set to "0" to disable).
const MUST_KEEP_ENV: &str = "HEADROOM_KOMPRESS_MUST_KEEP";

/// Add semantically fragile words that should never be model-dropped.
///
/// Mirrors Python `_add_kompress_must_keep_words`: scans `chunk_words` for
/// patterns the model might incorrectly score as unimportant (hex addresses,
/// file paths, CLI flags, error codes, etc.) and forces them into `kept_ids`.
fn add_must_keep_words(kept_ids: &mut BTreeSet<usize>, chunk_words: &[&str], chunk_start: usize) {
    let re = must_keep_re();
    for (word_idx, word) in chunk_words.iter().enumerate() {
        if re.is_match(word) {
            kept_ids.insert(word_idx + chunk_start);
        }
    }
}

/// Returns `true` if the must-keep safety net is enabled (default) or
/// disabled via `HEADROOM_KOMPRESS_MUST_KEEP=0`.
fn must_keep_enabled() -> bool {
    !std::env::var(MUST_KEEP_ENV)
        .map(|v| v == "0")
        .unwrap_or(false)
}

/// ONNX artifact candidates, tried in order. The first is a fp32 model whose
/// input shape is frozen to a static `[1, MAX_SEQ_LEN]` — required by the
/// OpenVINO **NPU** EP, which cannot compile dynamic `seq` (it hangs during
/// graph compilation on the dynamic-shape variants). When a static model is
/// loaded, `score_chunk` right-pads each chunk to its fixed length (detected
/// via [`detect_static_seq`]); dynamic models take the chunk's natural length
/// and pay no padding cost. The static model is absent from a vanilla install
/// (it is generated separately for NPU deployments), so this entry is simply
/// skipped on CPU/GPU. The remaining variants are the dynamic fall-throughs:
/// weight-only int8 (smallest; `MatMulNBits`, unsupported on NPU), then dynamic
/// fp32 (lossless reference), then the v1-era dynamic int8. A candidate is
/// skipped on download miss or on session-load failure.
pub const ONNX_CANDIDATES: &[&str] = &[
    "onnx/kompress-fp32-static512.onnx",
    "onnx/kompress-int8-wo.onnx",
    "onnx/kompress-fp32.onnx",
    "onnx/kompress-int8.onnx",
];

// ─── Types ──────────────────────────────────────────────────────────────

/// Configuration for [`Kompress`]. Field defaults match kompress-v2-base;
/// domain-specific models override `model_id` + `chunk_words` together.
#[derive(Debug, Clone)]
pub struct KompressConfig {
    pub model_id: String,
    pub tokenizer_repo: String,
    pub chunk_words: usize,
    pub score_threshold: f32,
    /// Configurable floor, clamped up to [`MIN_WORDS`] at the check.
    pub min_words: usize,
}

impl Default for KompressConfig {
    fn default() -> Self {
        Self {
            model_id: DEFAULT_MODEL_ID.to_string(),
            tokenizer_repo: DEFAULT_TOKENIZER_REPO.to_string(),
            chunk_words: DEFAULT_CHUNK_WORDS,
            score_threshold: DEFAULT_SCORE_THRESHOLD,
            min_words: DEFAULT_MIN_WORDS,
        }
    }
}

/// Result of a Kompress compression. Mirrors the Python `KompressResult`
/// fields that the proxy path populates (CCR `cache_key` is owned by the
/// dispatcher, not this engine).
#[derive(Debug, Clone, PartialEq)]
pub struct KompressResult {
    pub compressed: String,
    pub original: String,
    /// Whitespace-split word count of the input.
    pub original_tokens: usize,
    /// Word count of the output.
    pub compressed_tokens: usize,
    /// `compressed_tokens / original_tokens`, computed in f64 to match the
    /// Python reference's `float` division bit-for-bit.
    pub compression_ratio: f64,
    pub model_used: String,
}

impl KompressResult {
    /// Words dropped (never negative).
    pub fn tokens_saved(&self) -> usize {
        self.original_tokens.saturating_sub(self.compressed_tokens)
    }

    /// True when nothing was compressed (output == input word stream).
    pub fn is_passthrough(&self) -> bool {
        self.compressed_tokens == self.original_tokens
    }
}

#[derive(Debug, Error)]
pub enum KompressError {
    #[error("failed to load tokenizer for `{repo}`: {source}")]
    Tokenizer {
        repo: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("failed to download `{repo}` from HuggingFace Hub: {source}")]
    Hub {
        repo: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("no loadable ONNX artifact in `{model_id}` (tried {tried:?}): {source}")]
    Onnx {
        model_id: String,
        tried: Vec<String>,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

// ─── Compressor ─────────────────────────────────────────────────────────

/// A loaded Kompress model + tokenizer. Construct once (model load is
/// expensive) and share; `compress` takes `&self`.
///
/// ONNX inference is serialized behind a `Mutex` — matching the Python
/// reference, which caps ONNX execution to one concurrent call (the CPU
/// provider does not parallelize the batch dimension for this model).
pub struct Kompress {
    config: KompressConfig,
    tokenizer: Tokenizer,
    session: Mutex<Session>,
    /// `Some(n)` when the loaded ONNX has a **fixed** sequence dimension (a
    /// static `[1, n]` input), in which case `score_chunk` right-pads every
    /// chunk to `n`. `None` for the usual dynamic-`seq` models, which take the
    /// chunk's natural length. Detected from the session's `input_ids` shape
    /// (see [`detect_static_seq`]). The static path exists for execution
    /// providers that cannot compile dynamic shapes (OpenVINO NPU); masked
    /// padding leaves the real-token scores unchanged, so output is identical.
    static_seq: Option<usize>,
}

impl std::fmt::Debug for Kompress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kompress")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Inspect a built session's `input_ids` input: return `Some(n)` if its
/// sequence dimension is a fixed `n > 0` (a static-shape model), else `None`
/// (dynamic `seq`). ONNX inputs are `[batch, seq]`; a dynamic dim is reported
/// as `-1` by ONNX Runtime.
fn detect_static_seq(session: &Session) -> Option<usize> {
    let outlet = session.inputs().iter().find(|o| o.name() == "input_ids")?;
    let seq = *outlet.dtype().tensor_shape()?.get(1)?;
    (seq > 0).then_some(seq as usize)
}

impl Kompress {
    /// Wrap built artifacts into a `Kompress`, detecting whether the loaded
    /// model has a static sequence length (so `score_chunk` knows to pad).
    fn assemble(config: KompressConfig, tokenizer: Tokenizer, session: Session) -> Self {
        let static_seq = detect_static_seq(&session);
        Self {
            config,
            tokenizer,
            session: Mutex::new(session),
            static_seq,
        }
    }

    /// Build from local artifact paths — no network. Used by tests and
    /// the parity harness against the on-disk HuggingFace cache.
    pub fn from_files(
        tokenizer_path: impl AsRef<Path>,
        onnx_path: impl AsRef<Path>,
        config: KompressConfig,
    ) -> Result<Self, KompressError> {
        let tokenizer = load_tokenizer(tokenizer_path.as_ref(), &config.tokenizer_repo)?;
        let session = build_session(onnx_path.as_ref()).map_err(|e| KompressError::Onnx {
            model_id: config.model_id.clone(),
            tried: vec![onnx_path.as_ref().display().to_string()],
            source: e,
        })?;
        Ok(Self::assemble(config, tokenizer, session))
    }

    /// Build by resolving artifacts from the HuggingFace Hub (cache-first,
    /// downloading on miss). Blocking — call off the hot path. Tries the
    /// [`ONNX_CANDIDATES`] in order.
    pub fn from_pretrained(config: KompressConfig) -> Result<Self, KompressError> {
        let api = hf_hub::api::sync::Api::new().map_err(|e| KompressError::Hub {
            repo: config.model_id.clone(),
            source: Box::new(e),
        })?;

        let tok_path = api
            .model(config.tokenizer_repo.clone())
            .get("tokenizer.json")
            .map_err(|e| KompressError::Hub {
                repo: config.tokenizer_repo.clone(),
                source: Box::new(e),
            })?;
        let tokenizer = load_tokenizer(&tok_path, &config.tokenizer_repo)?;

        let model_api = api.model(config.model_id.clone());
        let mut last_err: Option<Box<dyn std::error::Error + Send + Sync>> = None;
        let mut tried: Vec<String> = Vec::new();
        for candidate in ONNX_CANDIDATES {
            tried.push((*candidate).to_string());
            let onnx_path: PathBuf = match model_api.get(candidate) {
                Ok(p) => p,
                Err(e) => {
                    last_err = Some(Box::new(e));
                    continue;
                }
            };
            match build_session(&onnx_path) {
                Ok(session) => {
                    return Ok(Self::assemble(config, tokenizer, session));
                }
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            }
        }
        Err(KompressError::Onnx {
            model_id: config.model_id.clone(),
            tried,
            source: last_err.unwrap_or_else(|| "no ONNX candidates configured".to_string().into()),
        })
    }

    /// Cache-only construction: resolve the tokenizer + ONNX artifact from
    /// the local HuggingFace cache **without ever hitting the network**, and
    /// return `Ok(None)` when they are not present (or no candidate loads).
    ///
    /// This is the Rust mirror of the Python reference's
    /// `allow_download=False` path (`KompressModelNotCached` → defer): it lets
    /// the live-zone dispatcher attempt a load on a hot path without risking a
    /// blocking 261 MB download. When the model isn't cached the caller passes
    /// plain text through untouched, exactly as Python does when Kompress is
    /// unavailable.
    pub fn from_cache(config: KompressConfig) -> Result<Option<Self>, KompressError> {
        let Some(tok_path) = hf_cache_file(&config.tokenizer_repo, &["tokenizer.json"]) else {
            // Diagnostic: a `None` here is the #1 cause of a silent
            // `kompress_ready=false`. Name the repo + the roots searched so
            // operators don't have to guess between "not downloaded" and
            // "present but unreadable" (e.g. HF symlinks over `\\wsl$`, which
            // native Windows can't follow — `path.exists()` returns false on
            // the unresolved symlink). Cache-only, so this is a defer, not an
            // error: the caller passes plain text through.
            tracing::warn!(
                event = "kompress_cache_miss",
                stage = "tokenizer",
                tokenizer_repo = %config.tokenizer_repo,
                searched_roots = ?hf_hub_roots(),
                "Kompress deferred: tokenizer.json not found in HF cache \
                 (not downloaded, or present but unreadable — e.g. HF symlinks \
                 over \\\\wsl$ which native Windows cannot follow)"
            );
            return Ok(None);
        };
        let tokenizer = load_tokenizer(&tok_path, &config.tokenizer_repo)?;
        let mut found_onnx = false;
        for candidate in ONNX_CANDIDATES {
            let rel: Vec<&str> = candidate.split('/').collect();
            let Some(onnx_path) = hf_cache_file(&config.model_id, &rel) else {
                continue;
            };
            found_onnx = true;
            match build_session(&onnx_path) {
                Ok(session) => {
                    return Ok(Some(Self::assemble(config, tokenizer, session)));
                }
                Err(e) => {
                    // The ONNX file is present but the session would not
                    // build — e.g. the active ORT execution provider rejects
                    // the graph (OpenVINO/NPU cannot compile the int8
                    // weight-only `MatMulNBits` op). Loudly surface it and try
                    // the next candidate (fp32) rather than die silently.
                    tracing::warn!(
                        event = "kompress_session_build_failed",
                        candidate = %candidate,
                        onnx_path = %onnx_path.display(),
                        error = %e,
                        "Kompress: ONNX found but session build failed; trying next candidate"
                    );
                }
            }
        }
        tracing::warn!(
            event = "kompress_cache_miss",
            stage = "onnx",
            model_id = %config.model_id,
            candidates = ?ONNX_CANDIDATES,
            any_onnx_found = found_onnx,
            searched_roots = ?hf_hub_roots(),
            "Kompress deferred: no usable ONNX session \
             (no candidate file in cache, or every candidate failed to build)"
        );
        Ok(None)
    }

    /// Model-decides compression (the proxy path): keep words scoring
    /// above `config.score_threshold`.
    pub fn compress(&self, content: &str) -> KompressResult {
        self.compress_inner(content, None)
    }

    /// Forced-ratio compression: keep the top `target_ratio` fraction of
    /// words by score (at least one). `None` defers to the threshold path.
    /// The proxy never sets this — only the user-facing API does.
    pub fn compress_with_ratio(&self, content: &str, target_ratio: Option<f64>) -> KompressResult {
        self.compress_inner(content, target_ratio)
    }

    fn compress_inner(&self, content: &str, target_ratio: Option<f64>) -> KompressResult {
        let words: Vec<&str> = content.split_whitespace().collect();
        let n_words = words.len();
        if n_words < self.config.min_words.max(MIN_WORDS) {
            return self.passthrough(content, n_words);
        }

        let mut kept_ids: BTreeSet<usize> = BTreeSet::new();
        let mut chunk_start = 0usize;
        while chunk_start < n_words {
            let end = (chunk_start + self.config.chunk_words).min(n_words);
            let chunk_words = &words[chunk_start..end];
            match self.score_chunk(chunk_words) {
                Ok(word_scores) => {
                    self.select_words(
                        &word_scores,
                        chunk_words,
                        chunk_start,
                        target_ratio,
                        &mut kept_ids,
                    );
                }
                Err(_) => {
                    // A chunk that fails inference is treated as
                    // "nothing salient here" — matches the Python
                    // reference's per-call passthrough-on-error.
                    return self.passthrough(content, n_words);
                }
            }
            chunk_start += self.config.chunk_words;
        }

        if kept_ids.is_empty() {
            return self.passthrough(content, n_words);
        }

        let compressed_words: Vec<&str> = kept_ids
            .iter()
            .filter(|&&w| w < n_words)
            .map(|&w| words[w])
            .collect();
        let compressed_tokens = compressed_words.len();
        let compressed = compressed_words.join(" ");
        let compression_ratio = if n_words == 0 {
            1.0
        } else {
            compressed_tokens as f64 / n_words as f64
        };
        KompressResult {
            compressed,
            original: content.to_string(),
            original_tokens: n_words,
            compressed_tokens,
            compression_ratio,
            model_used: self.config.model_id.clone(),
        }
    }

    /// Tokenize one chunk of words and return the **max score per word**
    /// (`word_index -> score`). `word_index` is local to the chunk.
    fn score_chunk(
        &self,
        chunk_words: &[&str],
    ) -> Result<HashMap<usize, f32>, Box<dyn std::error::Error + Send + Sync>> {
        let seq_in: Vec<&str> = chunk_words.to_vec();
        let encoding = self
            .tokenizer
            .encode(EncodeInput::Single(InputSequence::from(seq_in)), true)?;
        let ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
        let attn: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&x| x as i64)
            .collect();
        let word_ids = encoding.get_word_ids();
        let mut ids = ids;
        let mut attn = attn;

        // Static-shape models (e.g. the OpenVINO NPU build, which cannot
        // compile a dynamic `seq`) require a fixed `[1, static_seq]` input, so
        // right-pad every chunk to that length. Real tokens occupy
        // `0..real_seq`; the tail is padding with `attention_mask = 0`, which
        // masks those positions out of self-attention — the scores at real
        // positions are identical to an unpadded run, so keep/discard decisions
        // (hence parity) are unchanged. The tokenizer truncates to
        // `MAX_SEQ_LEN`, so the chunk never exceeds a `static_seq` of that size.
        // Dynamic models (`static_seq == None`) take the chunk's natural length
        // and pay no padding cost — the default for CPU/GPU.
        let seq = match self.static_seq {
            Some(n) => {
                debug_assert!(ids.len() <= n);
                ids.resize(n, 0);
                attn.resize(n, 0);
                n
            }
            None => ids.len(),
        };

        let input_ids = Tensor::from_array(([1usize, seq], ids))?;
        let attention_mask = Tensor::from_array(([1usize, seq], attn))?;

        let scores: Vec<f32> = {
            let mut session = self
                .session
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let outputs = session.run(ort::inputs![
                "input_ids" => input_ids,
                "attention_mask" => attention_mask
            ])?;
            let (_shape, data) = outputs["final_scores"].try_extract_tensor::<f32>()?;
            data.to_vec()
        };

        let mut word_scores: HashMap<usize, f32> = HashMap::new();
        for (idx, wid) in word_ids.iter().enumerate() {
            let Some(w) = wid else { continue };
            let Some(&s) = scores.get(idx) else { continue };
            let entry = word_scores.entry(*w as usize).or_insert(f32::MIN);
            if s > *entry {
                *entry = s;
            }
        }
        Ok(word_scores)
    }

    /// Apply the threshold or top-k rule to one chunk's per-word scores,
    /// inserting kept **global** word indices into `kept_ids`.
    fn select_words(
        &self,
        word_scores: &HashMap<usize, f32>,
        chunk_words: &[&str],
        chunk_start: usize,
        target_ratio: Option<f64>,
        kept_ids: &mut BTreeSet<usize>,
    ) {
        if word_scores.is_empty() {
            return;
        }
        match target_ratio {
            Some(ratio) => {
                // Stable top-k: iterate words in ascending index order so
                // equal scores break toward the lower word index — this
                // matches CPython's stable `sorted()` over the
                // insertion-ordered score dict (tokens emitted in word
                // order).
                let mut ordered: Vec<(usize, f32)> =
                    word_scores.iter().map(|(&w, &s)| (w, s)).collect();
                ordered.sort_by_key(|&(w, _)| w);
                ordered.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let num_keep = ((ordered.len() as f64 * ratio) as usize).max(1);
                for &(w, _) in ordered.iter().take(num_keep) {
                    kept_ids.insert(w + chunk_start);
                }
            }
            None => {
                for (&w, &s) in word_scores {
                    if s > self.config.score_threshold {
                        kept_ids.insert(w + chunk_start);
                    }
                }
            }
        }
        // Force-keep semantically fragile words (hex, paths, flags, etc.)
        // regardless of model score. Matches Python `_add_kompress_must_keep_words`.
        // Disable with HEADROOM_KOMPRESS_MUST_KEEP=0.
        if must_keep_enabled() {
            add_must_keep_words(kept_ids, chunk_words, chunk_start);
        }
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

    /// Expose the active config (read-only).
    pub fn config(&self) -> &KompressConfig {
        &self.config
    }
}

// ─── Loading helpers ────────────────────────────────────────────────────

fn load_tokenizer(path: &Path, repo: &str) -> Result<Tokenizer, KompressError> {
    let mut tokenizer = Tokenizer::from_file(path).map_err(|e| KompressError::Tokenizer {
        repo: repo.to_string(),
        source: e,
    })?;
    // Match the Python reference: truncation=True, max_length=512.
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: MAX_SEQ_LEN,
            ..Default::default()
        }))
        .map_err(|e| KompressError::Tokenizer {
            repo: repo.to_string(),
            source: e,
        })?;
    Ok(tokenizer)
}

/// Read an env var as `usize`, returning `None` when absent, empty, or
/// non-positive. Mirrors Python `_env_int`.
fn env_usize(name: &str) -> Option<usize> {
    let raw = std::env::var(name).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let value: usize = raw.parse().ok()?;
    if value == 0 {
        return None;
    }
    Some(value)
}

fn build_session(path: &Path) -> Result<Session, Box<dyn std::error::Error + Send + Sync>> {
    // `ort` is built with `load-dynamic`, so the ONNX Runtime shared library
    // must be resolved and committed BEFORE any session is constructed. This
    // is not a soft failure to skip: when the dylib cannot be loaded, ort
    // 2.0.0-rc.12 deadlocks inside its API-setup error path (recursive
    // `OnceLock` init) and the stuck thread then wedges process exit in
    // `dl_fini` (#1715). fastembed (`relevance::embedding`) and magika both
    // gate on this; Kompress reaches ORT by a third path and must too.
    //
    // Returning `Err` here degrades Kompress to passthrough, the same as a
    // cold model cache — the caller already treats a load failure that way.
    crate::transforms::magika_detector::dynamic_ort_loader_ready()?;

    let builder = Session::builder()?;

    // Apply ONNX thread configuration from env vars (mirrors Python
    // `_onnx_session_options` which reads HEADROOM_KOMPRESS_ONNX_INTRA_THREADS
    // and HEADROOM_KOMPRESS_ONNX_INTER_THREADS).
    //
    // These methods consume self and return Result. Thread configuration
    // is a best-effort hint that should never fail in practice, so we log
    // and continue on failure rather than aborting session creation.
    let builder = match env_usize("HEADROOM_KOMPRESS_ONNX_INTRA_THREADS") {
        Some(n) => match builder.with_intra_threads(n) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    event = "kompress_session_threads_failed",
                    threads = n,
                    error = %e,
                    "Failed to set intra-op threads; using ONNX Runtime default"
                );
                Session::builder()?
            }
        },
        None => builder,
    };

    let mut builder = match env_usize("HEADROOM_KOMPRESS_ONNX_INTER_THREADS") {
        Some(n) => match builder.with_inter_threads(n) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    event = "kompress_session_threads_failed",
                    threads = n,
                    error = %e,
                    "Failed to set inter-op threads; using ONNX Runtime default"
                );
                Session::builder()?
            }
        },
        None => builder,
    };

    let session = builder.commit_from_file(path)?;
    Ok(session)
}

/// Resolve `rel` (e.g. `["tokenizer.json"]` or `["onnx", "kompress-int8-wo.onnx"]`)
/// inside the local HuggingFace cache for `repo` (`"owner/name"`), searching
/// every snapshot under every candidate cache root. Returns `None` if not
/// present — never touches the network.
fn hf_cache_file(repo: &str, rel: &[&str]) -> Option<PathBuf> {
    let repo_dir = format!("models--{}", repo.replace('/', "--"));
    for hub in hf_hub_roots() {
        let snapshots = hub.join(&repo_dir).join("snapshots");
        let Ok(entries) = std::fs::read_dir(&snapshots) else {
            continue;
        };
        for snap in entries.flatten() {
            let mut cand = snap.path();
            for part in rel {
                cand = cand.join(part);
            }
            if cand.exists() {
                return Some(cand);
            }
        }
    }
    None
}

/// HuggingFace hub cache roots in resolution precedence. Cross-platform so
/// the cache-only loader works on Windows (native `headroom-proxy.exe`) as
/// well as Linux: `HF_HUB_CACHE` (the hub dir directly) → `HF_HOME/hub` →
/// `{HOME|USERPROFILE}/.cache/huggingface/hub`. `HOME` is the unix home; on
/// Windows the process sees `USERPROFILE` (and often no `HOME`), so both are
/// tried. Honoring `HF_HOME` also lets a Windows proxy point at a WSL cache.
fn hf_hub_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let push_env = |roots: &mut Vec<PathBuf>, var: &str, suffix: &[&str]| {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                let mut p = PathBuf::from(v);
                for s in suffix {
                    p = p.join(s);
                }
                roots.push(p);
            }
        }
    };
    push_env(&mut roots, "HF_HUB_CACHE", &[]);
    push_env(&mut roots, "HF_HOME", &["hub"]);
    push_env(&mut roots, "HOME", &[".cache", "huggingface", "hub"]);
    push_env(&mut roots, "USERPROFILE", &[".cache", "huggingface", "hub"]);
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_match_kompress_v2_base() {
        let c = KompressConfig::default();
        assert_eq!(c.model_id, "chopratejas/kompress-v2-base");
        assert_eq!(c.tokenizer_repo, "answerdotai/ModernBERT-base");
        assert_eq!(c.chunk_words, 350);
        assert_eq!(c.score_threshold, 0.5);
        assert_eq!(c.min_words, 64);
    }

    /// The floor is configurable, but never below the historical hard clamp:
    /// under it the model has too little signal to score anything usefully.
    #[test]
    fn a_floor_below_the_hard_clamp_is_raised_to_it() {
        let config = KompressConfig {
            min_words: 0,
            ..Default::default()
        };
        assert_eq!(config.min_words.max(MIN_WORDS), MIN_WORDS);
    }

    #[test]
    fn must_keep_regex_matches_expected_patterns() {
        let re = must_keep_re();
        // Hex addresses
        assert!(re.is_match("0x7fff2038"));
        assert!(re.is_match("0xDEADBEEF"));
        // Standalone numbers
        assert!(re.is_match("42"));
        assert!(re.is_match("3.14"));
        assert!(!re.is_match("abc123")); // not standalone
                                         // ALLCAPS
        assert!(re.is_match("SIGILL"));
        assert!(re.is_match("HTTP"));
        assert!(re.is_match("EOF"));
        assert!(!re.is_match("Hi")); // too short
                                     // Dotted paths
        assert!(re.is_match("libsystem_kernel.dylib"));
        assert!(re.is_match("com.example.MyClass"));
        // Unix paths
        assert!(re.is_match("/usr/lib/python3.so"));
        assert!(re.is_match("/tmp/test"));
        // Extensions
        assert!(re.is_match(".py"));
        assert!(re.is_match(".json"));
        assert!(!re.is_match(".a")); // too short
                                     // CLI flags
        assert!(re.is_match("--verbose"));
        assert!(re.is_match("-n"));
        assert!(!re.is_match("--")); // no letters after
                                     // CamelCase
        assert!(re.is_match("EXC_BAD_INSTRUCTION"));
        assert!(re.is_match("IndexError"));
        // Note: "HTTPServer" contains "HTTP" which matches ALLCAPS pattern -
        // this is correct behavior as the ALLCAPS substring is semantically
        // significant and should be preserved.
    }

    #[test]
    fn add_must_keep_words_forces_fragile_words() {
        let mut kept = BTreeSet::new();
        let words = vec!["the", "path", "/usr/lib/python3.so", "was", "0x7fff2038"];
        add_must_keep_words(&mut kept, &words, 10);
        // Indices 2 and 4 (relative to chunk_start=10) should be forced
        assert!(kept.contains(&12)); // /usr/lib/python3.so
        assert!(kept.contains(&14)); // 0x7fff2038
        assert!(!kept.contains(&10)); // "the" not matched
    }

    #[test]
    fn add_must_keep_words_disabled_via_env() {
        std::env::set_var(MUST_KEEP_ENV, "0");
        assert!(!must_keep_enabled());
        std::env::remove_var(MUST_KEEP_ENV);
    }

    #[test]
    fn env_usize_helper() {
        assert_eq!(env_usize("NONEXISTENT_VAR_12345"), None);
        std::env::set_var("HEADROOM_TEST_ENV_USIZE", "42");
        assert_eq!(env_usize("HEADROOM_TEST_ENV_USIZE"), Some(42));
        std::env::set_var("HEADROOM_TEST_ENV_USIZE", "0");
        assert_eq!(env_usize("HEADROOM_TEST_ENV_USIZE"), None);
        std::env::set_var("HEADROOM_TEST_ENV_USIZE", "not_a_number");
        assert_eq!(env_usize("HEADROOM_TEST_ENV_USIZE"), None);
        std::env::set_var("HEADROOM_TEST_ENV_USIZE", "");
        assert_eq!(env_usize("HEADROOM_TEST_ENV_USIZE"), None);
        std::env::remove_var("HEADROOM_TEST_ENV_USIZE");
    }

    #[test]
    fn result_helpers() {
        let r = KompressResult {
            compressed: "a b".into(),
            original: "a b c d".into(),
            original_tokens: 4,
            compressed_tokens: 2,
            compression_ratio: 0.5,
            model_used: DEFAULT_MODEL_ID.into(),
        };
        assert_eq!(r.tokens_saved(), 2);
        assert!(!r.is_passthrough());

        let p = KompressResult {
            compressed: "a b".into(),
            original: "a b".into(),
            original_tokens: 2,
            compressed_tokens: 2,
            compression_ratio: 1.0,
            model_used: DEFAULT_MODEL_ID.into(),
        };
        assert_eq!(p.tokens_saved(), 0);
        assert!(p.is_passthrough());
    }
}
