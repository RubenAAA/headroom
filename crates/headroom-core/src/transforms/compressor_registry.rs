//! Pluggable compressor registry — Rust port of `headroom.transforms.compressor_registry`.
//!
//! This is the *name-addressable* seam for compressors: built-in compressors are
//! registered explicitly, and a caller opts in to a specific set of them by name.
//!
//! # Pure-data contract
//!
//! The compressor boundary is deliberately **pure data in / data out**. No
//! tokenizer instances, live store handles, or rich config types cross it —
//! every field is a string, integer, bool, or a collection of those. Python's
//! module documents this as its "Rust-portable contract", and this module is
//! the other end of it: the shapes match field-for-field so the same compressor
//! contract holds in either language.
//!
//! # Discovery is explicit here, not entry-point driven
//!
//! Python's [`CompressorRegistry.discover`] enumerates the `headroom.compressor`
//! entry-point group via `importlib.metadata`, loading compressors out of any
//! installed third-party package. Rust has no runtime equivalent — a compiled
//! binary cannot load a crate that was not linked into it — so that method has
//! no counterpart here and registration is always explicit via [`register`].
//!
//! This is a genuine capability difference, not an oversight: an out-of-tree
//! Rust compressor must be linked in and registered by the embedding binary. The
//! opt-in *selection* half of the model ([`select`] / [`active`]) ports exactly,
//! and is what keeps a registered compressor inert until it is named.
//!
//! [`CompressorRegistry.discover`]: https://docs.python.org/3/library/importlib.metadata.html
//! [`register`]: CompressorRegistry::register
//! [`select`]: CompressorRegistry::select
//! [`active`]: CompressorRegistry::active

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Recognized values for [`CompressorDescriptor::cost_tier`].
pub const COST_TIERS: [&str; 3] = ["fast", "ml", "remote"];

/// Static, declarative metadata describing a compressor's capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressorDescriptor {
    /// Canonical, unique name used for registration and selection.
    pub name: String,
    /// Content types this compressor handles (e.g. `["text/plain"]`).
    pub content_types: Vec<String>,
    /// Whether compression is losslessly reversible.
    pub lossless: bool,
    /// One of [`COST_TIERS`] — `"fast"` (local/cheap), `"ml"` (local model
    /// inference), or `"remote"` (network call).
    pub cost_tier: String,
    /// Whether the compressor can emit a `hash -> original` recovery map in
    /// [`CompressOutput::recoverable`].
    pub recoverable: bool,
}

/// Pure-data input to [`Compressor::compress`].
#[derive(Debug, Clone, Default)]
pub struct CompressInput {
    /// The raw content to compress.
    pub content: String,
    /// The content type of `content`.
    pub content_type: String,
    /// Optional task/query hint for relevance-aware compressors.
    pub query: String,
    /// Plain compressor-specific configuration.
    pub config: BTreeMap<String, String>,
    /// Plain budget hints, e.g. `bias`.
    pub budget: BTreeMap<String, String>,
}

/// Pure-data output from [`Compressor::compress`].
#[derive(Debug, Clone)]
pub struct CompressOutput {
    /// The compressed content, or the original unchanged when `compressed` is
    /// `false`.
    pub content: String,
    /// Token count of the input content.
    pub tokens_before: usize,
    /// Token count of the compressed content.
    pub tokens_after: usize,
    /// Whether this particular result is losslessly reversible.
    pub lossless: bool,
    /// Marker strings describing what was applied.
    pub markers: Vec<String>,
    /// `hash -> original` map for recovering dropped content.
    pub recoverable: BTreeMap<String, String>,
    /// Non-fatal warnings emitted during compression.
    pub warnings: Vec<String>,
    /// Whether the compressor actually compressed the content.
    ///
    /// `true` (the default) means compression was applied and [`content`] is the
    /// transformed result; `false` means passthrough and [`content`] is the
    /// original input unchanged. Callers read this to distinguish a real (but
    /// possibly no-shrink) result from a passthrough, and run their own fallback
    /// on a passthrough.
    ///
    /// [`content`]: CompressOutput::content
    pub compressed: bool,
}

impl Default for CompressOutput {
    fn default() -> Self {
        Self {
            content: String::new(),
            tokens_before: 0,
            tokens_after: 0,
            lossless: false,
            markers: Vec::new(),
            recoverable: BTreeMap::new(),
            warnings: Vec::new(),
            // Mirrors Python's `compressed: bool = True` default, so a
            // compressor that always transforms need not set it.
            compressed: true,
        }
    }
}

/// Name-addressable compressor contract (pure data in / data out).
///
/// A compressor that does not compress its input (a passthrough) returns
/// `CompressOutput { content: input.content.clone(), compressed: false, .. }`.
pub trait Compressor: Send + Sync {
    /// This compressor's static capability metadata.
    fn descriptor(&self) -> &CompressorDescriptor;

    /// Compress `input`.
    fn compress(&self, input: &CompressInput) -> CompressOutput;
}

/// Why a registration was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// The descriptor's name was empty.
    EmptyName,
    /// A compressor of this name is already registered and `replace` was false.
    AlreadyRegistered(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::EmptyName => write!(f, "compressor descriptor.name must be non-empty"),
            RegistryError::AlreadyRegistered(name) => {
                write!(f, "compressor {name:?} is already registered")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// Registry of compressors addressable by [`CompressorDescriptor::name`].
///
/// Starts empty; selection is opt-in. A `BTreeMap` backs it so `names()` and
/// `descriptors()` are sorted by construction, matching Python's explicit
/// `sorted()` calls.
#[derive(Default)]
pub struct CompressorRegistry {
    compressors: BTreeMap<String, Arc<dyn Compressor>>,
}

impl CompressorRegistry {
    /// A registry with no compressors.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `compressor` under its `descriptor.name`.
    ///
    /// Returns the registered name, or [`RegistryError`] if the name is empty or
    /// already taken and `replace` is false.
    pub fn register(
        &mut self,
        compressor: Arc<dyn Compressor>,
        replace: bool,
    ) -> Result<String, RegistryError> {
        let name = compressor.descriptor().name.clone();
        if name.is_empty() {
            return Err(RegistryError::EmptyName);
        }
        if self.compressors.contains_key(&name) && !replace {
            return Err(RegistryError::AlreadyRegistered(name));
        }
        self.compressors.insert(name.clone(), compressor);
        Ok(name)
    }

    /// The registered compressor named `name`, if any.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Compressor>> {
        self.compressors.get(name)
    }

    /// All registered compressor names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.compressors.keys().cloned().collect()
    }

    /// The descriptors of all registered compressors, sorted by name.
    pub fn descriptors(&self) -> Vec<CompressorDescriptor> {
        self.compressors
            .values()
            .map(|c| c.descriptor().clone())
            .collect()
    }

    /// Normalize a requested selection: strip whitespace, drop empties.
    fn resolve_selection(names: Option<&[String]>) -> BTreeSet<String> {
        names
            .unwrap_or(&[])
            .iter()
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Resolve an opt-in selection to the set of *active* registered names.
    ///
    /// - `None` or an empty selection selects nothing (the opt-in default).
    /// - The literal `"*"` selects every registered compressor.
    /// - Otherwise only names that are both requested and registered are active;
    ///   requested-but-unregistered names are logged and skipped.
    ///
    /// Never invokes `compress` — this only decides which already-registered
    /// compressors are active.
    pub fn select(&self, names: Option<&[String]>) -> BTreeSet<String> {
        let requested = Self::resolve_selection(names);
        let registered: BTreeSet<String> = self.compressors.keys().cloned().collect();

        if requested.is_empty() {
            if !registered.is_empty() {
                tracing::info!(
                    registered = %registered.iter().cloned().collect::<Vec<_>>().join(","),
                    "compressors registered but none selected (opt-in). \
                     Select by name or use '*' for all."
                );
            }
            return BTreeSet::new();
        }

        if requested.contains("*") {
            return registered;
        }

        let missing: Vec<String> = requested.difference(&registered).cloned().collect();
        if !missing.is_empty() {
            let available = registered.iter().cloned().collect::<Vec<_>>().join(",");
            tracing::warn!(
                missing = %missing.join(","),
                available = %if available.is_empty() { "<none>".to_string() } else { available },
                "compressors requested but not registered"
            );
        }
        requested.intersection(&registered).cloned().collect()
    }

    /// The active compressor objects for `selection`, sorted by name.
    ///
    /// `selection` is the raw opt-in request; it is resolved via [`select`].
    ///
    /// [`select`]: CompressorRegistry::select
    pub fn active(&self, selection: Option<&[String]>) -> Vec<Arc<dyn Compressor>> {
        self.select(selection)
            .iter()
            .filter_map(|name| self.compressors.get(name).cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubCompressor {
        descriptor: CompressorDescriptor,
    }

    impl StubCompressor {
        fn new(name: &str) -> Arc<dyn Compressor> {
            Arc::new(Self {
                descriptor: CompressorDescriptor {
                    name: name.to_string(),
                    content_types: vec!["text/plain".to_string()],
                    lossless: true,
                    cost_tier: "fast".to_string(),
                    recoverable: false,
                },
            })
        }
    }

    impl Compressor for StubCompressor {
        fn descriptor(&self) -> &CompressorDescriptor {
            &self.descriptor
        }

        fn compress(&self, input: &CompressInput) -> CompressOutput {
            CompressOutput {
                content: input.content.to_uppercase(),
                ..Default::default()
            }
        }
    }

    fn sel(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn register_returns_the_descriptor_name() {
        let mut registry = CompressorRegistry::new();
        let name = registry
            .register(StubCompressor::new("alpha"), false)
            .unwrap();
        assert_eq!(name, "alpha");
        assert!(registry.get("alpha").is_some());
    }

    #[test]
    fn register_rejects_an_empty_name() {
        let mut registry = CompressorRegistry::new();
        let err = registry
            .register(StubCompressor::new(""), false)
            .unwrap_err();
        assert_eq!(err, RegistryError::EmptyName);
    }

    #[test]
    fn register_rejects_a_duplicate_unless_replace_is_set() {
        let mut registry = CompressorRegistry::new();
        registry
            .register(StubCompressor::new("alpha"), false)
            .unwrap();

        let err = registry
            .register(StubCompressor::new("alpha"), false)
            .unwrap_err();
        assert_eq!(err, RegistryError::AlreadyRegistered("alpha".to_string()));

        registry
            .register(StubCompressor::new("alpha"), true)
            .unwrap();
        assert_eq!(registry.names(), vec!["alpha".to_string()]);
    }

    #[test]
    fn names_and_descriptors_are_sorted() {
        let mut registry = CompressorRegistry::new();
        for name in ["zulu", "alpha", "mike"] {
            registry.register(StubCompressor::new(name), false).unwrap();
        }
        assert_eq!(registry.names(), sel(&["alpha", "mike", "zulu"]));
        let descriptor_names: Vec<String> =
            registry.descriptors().into_iter().map(|d| d.name).collect();
        assert_eq!(descriptor_names, sel(&["alpha", "mike", "zulu"]));
    }

    /// The core safety property: installing a compressor must not activate it.
    #[test]
    fn selection_is_opt_in_so_none_is_active_by_default() {
        let mut registry = CompressorRegistry::new();
        registry
            .register(StubCompressor::new("alpha"), false)
            .unwrap();

        assert!(registry.select(None).is_empty());
        assert!(registry.select(Some(&[])).is_empty());
        assert!(registry.active(None).is_empty());
    }

    #[test]
    fn wildcard_selects_everything_registered() {
        let mut registry = CompressorRegistry::new();
        registry
            .register(StubCompressor::new("alpha"), false)
            .unwrap();
        registry
            .register(StubCompressor::new("bravo"), false)
            .unwrap();

        let selected = registry.select(Some(&sel(&["*"])));
        assert_eq!(
            selected.into_iter().collect::<Vec<_>>(),
            sel(&["alpha", "bravo"])
        );
    }

    #[test]
    fn unregistered_names_are_skipped_not_fatal() {
        let mut registry = CompressorRegistry::new();
        registry
            .register(StubCompressor::new("alpha"), false)
            .unwrap();

        let selected = registry.select(Some(&sel(&["alpha", "ghost"])));
        assert_eq!(selected.into_iter().collect::<Vec<_>>(), sel(&["alpha"]));
    }

    #[test]
    fn selection_entries_are_trimmed_and_blanks_dropped() {
        let mut registry = CompressorRegistry::new();
        registry
            .register(StubCompressor::new("alpha"), false)
            .unwrap();

        let selected = registry.select(Some(&sel(&["  alpha  ", "   "])));
        assert_eq!(selected.into_iter().collect::<Vec<_>>(), sel(&["alpha"]));

        // A selection of nothing but blanks is an empty selection, not a wildcard.
        assert!(registry.select(Some(&sel(&["", "  "]))).is_empty());
    }

    #[test]
    fn active_returns_compressors_sorted_by_name() {
        let mut registry = CompressorRegistry::new();
        registry
            .register(StubCompressor::new("zulu"), false)
            .unwrap();
        registry
            .register(StubCompressor::new("alpha"), false)
            .unwrap();

        let active = registry.active(Some(&sel(&["*"])));
        let active_names: Vec<String> =
            active.iter().map(|c| c.descriptor().name.clone()).collect();
        assert_eq!(active_names, sel(&["alpha", "zulu"]));
    }

    /// `compressed` defaults to `true`, matching Python's dataclass default, so a
    /// compressor that always transforms need not set the flag.
    #[test]
    fn compress_output_defaults_to_compressed() {
        assert!(CompressOutput::default().compressed);
    }

    #[test]
    fn cost_tiers_match_python() {
        assert_eq!(COST_TIERS, ["fast", "ml", "remote"]);
    }
}
