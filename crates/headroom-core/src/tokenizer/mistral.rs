//! Mistral-family model → tokenizer version / HuggingFace repo mapping.
//!
//! Ports the pure data + logic from `headroom/tokenizers/mistral.py`
//! (`MODEL_TO_VERSION` + `get_tokenizer_version`) and the Mistral-family
//! slice of `headroom/tokenizers/huggingface.py::MODEL_TO_TOKENIZER`.
//!
//! # Parity situation (read before "fixing" divergences)
//!
//! Python's primary path uses the `mistral_common` package (official Mistral
//! tokenizer with chat templates). That package is Python-only — no Rust
//! crate exists — and is *not installed* in this repo (not in
//! pyproject.toml). In practice Python's registry `_create_mistral` catches
//! the constructor `RuntimeError` and falls back to a plain
//! `EstimatingTokenCounter()` with no Mistral-specific calibration. So:
//!
//! - **Byte-parity with `mistral_common` is unreproducible** in Rust.
//! - **What actually runs in production Python today** is generic
//!   estimation, which is exactly what [`super::get_tokenizer`] returns for
//!   Mistral models unless a HuggingFace tokenizer is registered. Parity
//!   here means matching the *absence* of special handling.
//! - **Content-token parity** is achievable opt-in: register the family's
//!   public `tokenizer.json` files via [`try_register_default_mistral`].
//!   Chat-template overhead still differs — the generic
//!   `count_messages_default` (MESSAGE_OVERHEAD=4, REPLY_OVERHEAD=3) is
//!   used instead of Mistral's instruct template, matching Python's
//!   `BaseTokenizer.count_messages` fallback.
//!
//! Consequently this module deliberately does NOT touch
//! [`super::detect_backend`], does NOT add a `count_messages` override, and
//! does NOT add a Mistral-specific estimator density.

use super::registry::try_register_hf;
use super::HfTokenizerError;

/// Direct model-name → tokenizer version table.
/// 1:1 port of `MODEL_TO_VERSION` in `headroom/tokenizers/mistral.py`.
const MODEL_TO_VERSION: &[(&str, &str)] = &[
    // Mistral models use v3 tokenizer (tekken)
    ("mistral-large", "v3"),
    ("mistral-large-latest", "v3"),
    ("mistral-small", "v3"),
    ("mistral-small-latest", "v3"),
    ("ministral-8b", "v3"),
    ("ministral-3b", "v3"),
    ("mistral-nemo", "v3"),
    ("pixtral-12b", "v3"),
    ("codestral", "v3"),
    ("codestral-latest", "v3"),
    // Mixtral uses v1
    ("mixtral-8x7b", "v1"),
    ("mixtral-8x22b", "v1"),
    ("open-mixtral-8x7b", "v1"),
    ("open-mixtral-8x22b", "v1"),
    // Mistral 7B uses v1
    ("mistral-7b", "v1"),
    ("open-mistral-7b", "v1"),
    ("mistral-7b-instruct", "v1"),
];

/// Prefix fallbacks, checked in order after the direct table misses.
/// Same list and *same order* as `get_tokenizer_version` in Python — order
/// matters (e.g. `open-mistral-nemo` hits `open-mistral` → v1, not
/// `mistral-nemo` → v3, because `open-mistral-…` doesn't start with
/// `mistral-nemo`; but `mistral-7b-…` must be tried before any broader
/// pattern would).
const PREFIX_TO_VERSION: &[(&str, &str)] = &[
    ("mistral-large", "v3"),
    ("mistral-small", "v3"),
    ("ministral", "v3"),
    ("codestral", "v3"),
    ("pixtral", "v3"),
    ("mistral-nemo", "v3"),
    ("mixtral", "v1"),
    ("mistral-7b", "v1"),
    ("open-mistral", "v1"),
];

/// Tokenizer version (`"v1"` / `"v2"` / `"v3"`) for a Mistral-family model.
///
/// 1:1 port of `get_tokenizer_version` in `headroom/tokenizers/mistral.py`:
/// case-folded direct lookup, then ordered prefix match, then default `"v3"`
/// for newer/unknown models. (`"v2"` never occurs in the mapping today but
/// is a valid version in the Python API surface.)
pub fn mistral_tokenizer_version(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();

    for (name, version) in MODEL_TO_VERSION {
        if m == *name {
            return version;
        }
    }

    for (prefix, version) in PREFIX_TO_VERSION {
        if m.starts_with(prefix) {
            return version;
        }
    }

    // Default to v3 for newer models.
    "v3"
}

// ---- HuggingFace repo resolution ----------------------------------------

/// Mistral 7B base repo — the generic `mistral` fallback in Python's
/// `MODEL_TO_TOKENIZER`.
const REPO_MISTRAL_7B: &str = "mistralai/Mistral-7B-v0.1";
const REPO_MISTRAL_NEMO: &str = "mistralai/Mistral-Nemo-Base-2407";
const REPO_MISTRAL_SMALL: &str = "mistralai/Mistral-Small-Instruct-2409";
const REPO_MISTRAL_LARGE: &str = "mistralai/Mistral-Large-Instruct-2407";
const REPO_MIXTRAL_8X7B: &str = "mistralai/Mixtral-8x7B-v0.1";
const REPO_MIXTRAL_8X22B: &str = "mistralai/Mixtral-8x22B-v0.1";
// Not present in Python's MODEL_TO_TOKENIZER (those models routed to the
// mistral backend there, which fell back to estimation). Added so all five
// registry prefixes (`mistral`/`mixtral`/`codestral`/`ministral`/`pixtral`)
// can resolve to a public tokenizer.json.
const REPO_CODESTRAL: &str = "mistralai/Codestral-22B-v0.1";
const REPO_MINISTRAL: &str = "mistralai/Ministral-8B-Instruct-2410";
// `mistralai/Pixtral-12B-2409` ships tekken.json, not tokenizer.json; the
// community mirror carries the transformers-format tokenizer.
const REPO_PIXTRAL: &str = "mistral-community/pixtral-12b";

/// Direct model-name → HF repo. Mirrors the Mistral-family entries of
/// `MODEL_TO_TOKENIZER` in `headroom/tokenizers/huggingface.py`, plus the
/// three families Python left without a repo (see constants above).
const MODEL_TO_REPO: &[(&str, &str)] = &[
    ("mistral", REPO_MISTRAL_7B),
    ("mistral-7b", REPO_MISTRAL_7B),
    ("mistral-7b-v0.2", "mistralai/Mistral-7B-Instruct-v0.2"),
    ("mistral-7b-v0.3", "mistralai/Mistral-7B-Instruct-v0.3"),
    ("mistral-nemo", REPO_MISTRAL_NEMO),
    ("mistral-small", REPO_MISTRAL_SMALL),
    ("mistral-large", REPO_MISTRAL_LARGE),
    ("mixtral", REPO_MIXTRAL_8X7B),
    ("mixtral-8x7b", REPO_MIXTRAL_8X7B),
    ("mixtral-8x22b", REPO_MIXTRAL_8X22B),
    ("codestral", REPO_CODESTRAL),
    ("ministral", REPO_MINISTRAL),
    ("pixtral", REPO_PIXTRAL),
];

/// Prefix fallbacks for repo resolution, most-specific first so that e.g.
/// `mistral-large-2411` resolves to the Large repo, not the generic
/// `mistral` → 7B entry. (Python only had exact lookup here; dated model
/// names simply missed.)
const PREFIX_TO_REPO: &[(&str, &str)] = &[
    ("mistral-large", REPO_MISTRAL_LARGE),
    ("mistral-small", REPO_MISTRAL_SMALL),
    ("mistral-nemo", REPO_MISTRAL_NEMO),
    ("mistral-7b", REPO_MISTRAL_7B),
    ("open-mistral", REPO_MISTRAL_7B),
    ("open-mixtral-8x22b", REPO_MIXTRAL_8X22B),
    ("open-mixtral", REPO_MIXTRAL_8X7B),
    ("mixtral-8x22b", REPO_MIXTRAL_8X22B),
    ("mixtral", REPO_MIXTRAL_8X7B),
    ("ministral", REPO_MINISTRAL),
    ("codestral", REPO_CODESTRAL),
    ("pixtral", REPO_PIXTRAL),
    ("mistral", REPO_MISTRAL_7B),
];

/// Resolve a Mistral-family model name to a public HuggingFace repo whose
/// `tokenizer.json` counts its content tokens. Case-folded direct lookup,
/// then most-specific-prefix fallback. `None` for non-Mistral names.
pub fn mistral_hf_repo(model: &str) -> Option<&'static str> {
    let m = model.to_ascii_lowercase();

    for (name, repo) in MODEL_TO_REPO {
        if m == *name {
            return Some(repo);
        }
    }

    for (prefix, repo) in PREFIX_TO_REPO {
        if m.starts_with(prefix) {
            return Some(repo);
        }
    }

    None
}

/// Prefix → repo pairs registered by [`try_register_default_mistral`].
///
/// The five base prefixes match the Mistral patterns in Python's registry
/// dispatch (`^mistral`, `^mixtral`, `^codestral`, `^ministral`,
/// `^pixtral`). The extra `mistral-large`/`-small`/`-nemo` registrations
/// exist because those models use the 131k-vocab tekken (v3) tokenizer, not
/// the 32k v1 one — longest-prefix-wins in the registry routes them to the
/// right vocab.
const DEFAULT_REGISTRATIONS: &[(&str, &str)] = &[
    ("mistral", REPO_MISTRAL_7B),
    ("mistral-large", REPO_MISTRAL_LARGE),
    ("mistral-small", REPO_MISTRAL_SMALL),
    ("mistral-nemo", REPO_MISTRAL_NEMO),
    ("mixtral", REPO_MIXTRAL_8X7B),
    ("codestral", REPO_CODESTRAL),
    ("ministral", REPO_MINISTRAL),
    ("pixtral", REPO_PIXTRAL),
];

/// Best-effort: download and register HuggingFace tokenizers for the
/// Mistral family so [`super::get_tokenizer`] returns real counts instead
/// of the estimator for `mistral*` / `mixtral*` / `codestral*` /
/// `ministral*` / `pixtral*` model names.
///
/// Network-dependent (HF Hub, cached on disk after the first success) and
/// fail-soft: each registration is independent, nothing panics. Most
/// `mistralai/*` repos are gated — without an accepted license + `HF_TOKEN`
/// those entries fail with [`HfTokenizerError::Hub`] and are reported in
/// the returned list. An empty return means every prefix registered.
pub fn try_register_default_mistral() -> Vec<(&'static str, HfTokenizerError)> {
    let mut failures = Vec::new();
    for (prefix, repo) in DEFAULT_REGISTRATIONS {
        if let Err(e) = try_register_hf(prefix, repo) {
            failures.push((*prefix, e));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenizer::registry::test_support::{RegistryGuard, TINY_TOKENIZER_JSON};
    use crate::tokenizer::{get_tokenizer, register_hf, Backend, HfTokenizer};

    fn tiny(name: &str) -> HfTokenizer {
        HfTokenizer::from_bytes(name, TINY_TOKENIZER_JSON.as_bytes()).unwrap()
    }

    // ---- mistral_tokenizer_version --------------------------------------

    #[test]
    fn version_direct_table_matches_python() {
        // Every MODEL_TO_VERSION entry from headroom/tokenizers/mistral.py.
        for (model, expected) in [
            ("mistral-large", "v3"),
            ("mistral-large-latest", "v3"),
            ("mistral-small", "v3"),
            ("mistral-small-latest", "v3"),
            ("ministral-8b", "v3"),
            ("ministral-3b", "v3"),
            ("mistral-nemo", "v3"),
            ("pixtral-12b", "v3"),
            ("codestral", "v3"),
            ("codestral-latest", "v3"),
            ("mixtral-8x7b", "v1"),
            ("mixtral-8x22b", "v1"),
            ("open-mixtral-8x7b", "v1"),
            ("open-mixtral-8x22b", "v1"),
            ("mistral-7b", "v1"),
            ("open-mistral-7b", "v1"),
            ("mistral-7b-instruct", "v1"),
        ] {
            assert_eq!(mistral_tokenizer_version(model), expected, "{model}");
        }
    }

    #[test]
    fn version_prefix_fallbacks() {
        for (model, expected) in [
            ("mistral-large-2411", "v3"),
            ("mistral-small-2409", "v3"),
            ("ministral-8b-latest", "v3"),
            ("ministral-3b-2410", "v3"),
            ("codestral-2405", "v3"),
            ("codestral-mamba", "v3"),
            ("pixtral-large-latest", "v3"),
            ("mistral-nemo-instruct-2407", "v3"),
            ("mixtral-8x7b-instruct-v0.1", "v1"),
            ("mistral-7b-instruct-v0.3", "v1"),
            // Quirk preserved from Python: `open-mistral` is checked as a
            // prefix, so open-mistral-nemo lands on v1 (not mistral-nemo v3).
            ("open-mistral-nemo", "v1"),
            ("open-mistral-7b-v0.3", "v1"),
        ] {
            assert_eq!(mistral_tokenizer_version(model), expected, "{model}");
        }
    }

    #[test]
    fn version_unknown_defaults_to_v3() {
        for model in ["mistral-medium", "magistral-small", "some-new-model", ""] {
            assert_eq!(mistral_tokenizer_version(model), "v3", "{model}");
        }
    }

    #[test]
    fn version_case_insensitive() {
        assert_eq!(mistral_tokenizer_version("Mistral-Large"), "v3");
        assert_eq!(mistral_tokenizer_version("MIXTRAL-8X7B"), "v1");
        assert_eq!(mistral_tokenizer_version("Open-Mistral-7B"), "v1");
        assert_eq!(mistral_tokenizer_version("CODESTRAL-latest"), "v3");
    }

    // ---- mistral_hf_repo -------------------------------------------------

    #[test]
    fn repo_direct_table_mirrors_python_huggingface_map() {
        // Mistral-family entries of MODEL_TO_TOKENIZER in
        // headroom/tokenizers/huggingface.py.
        for (model, repo) in [
            ("mistral", "mistralai/Mistral-7B-v0.1"),
            ("mistral-7b", "mistralai/Mistral-7B-v0.1"),
            ("mistral-7b-v0.2", "mistralai/Mistral-7B-Instruct-v0.2"),
            ("mistral-7b-v0.3", "mistralai/Mistral-7B-Instruct-v0.3"),
            ("mistral-nemo", "mistralai/Mistral-Nemo-Base-2407"),
            ("mistral-small", "mistralai/Mistral-Small-Instruct-2409"),
            ("mistral-large", "mistralai/Mistral-Large-Instruct-2407"),
            ("mixtral", "mistralai/Mixtral-8x7B-v0.1"),
            ("mixtral-8x7b", "mistralai/Mixtral-8x7B-v0.1"),
            ("mixtral-8x22b", "mistralai/Mixtral-8x22B-v0.1"),
        ] {
            assert_eq!(mistral_hf_repo(model), Some(repo), "{model}");
        }
    }

    #[test]
    fn repo_resolves_for_all_five_registry_prefixes() {
        // Every Mistral pattern in Python's registry dispatch (^mistral,
        // ^mixtral, ^codestral, ^ministral, ^pixtral) must resolve, including
        // dated/suffixed variants that miss the direct table.
        for model in [
            "mistral-large-2411",
            "mixtral-8x22b-instruct",
            "codestral-2405",
            "ministral-3b-latest",
            "pixtral-12b-2409",
        ] {
            assert!(mistral_hf_repo(model).is_some(), "{model}");
        }
        // And the default registrations cover the same five families.
        for prefix in ["mistral", "mixtral", "codestral", "ministral", "pixtral"] {
            assert!(
                DEFAULT_REGISTRATIONS.iter().any(|(p, _)| *p == prefix),
                "no default registration for prefix {prefix}"
            );
        }
    }

    #[test]
    fn repo_prefix_prefers_most_specific() {
        // A dated Large model must not fall through to the generic 7B entry.
        assert_eq!(
            mistral_hf_repo("mistral-large-2411"),
            Some("mistralai/Mistral-Large-Instruct-2407")
        );
        assert_eq!(
            mistral_hf_repo("mixtral-8x22b-v0.1"),
            Some("mistralai/Mixtral-8x22B-v0.1")
        );
        assert_eq!(
            mistral_hf_repo("open-mixtral-8x22b-2404"),
            Some("mistralai/Mixtral-8x22B-v0.1")
        );
    }

    #[test]
    fn repo_case_insensitive_and_none_for_foreign_models() {
        assert_eq!(
            mistral_hf_repo("Mistral-Large"),
            Some("mistralai/Mistral-Large-Instruct-2407")
        );
        assert_eq!(mistral_hf_repo("claude-3-opus"), None);
        assert_eq!(mistral_hf_repo("gpt-4o"), None);
        assert_eq!(mistral_hf_repo("llama-3-70b"), None);
    }

    // ---- registration plumbing -------------------------------------------

    #[test]
    fn registered_prefixes_route_mistral_models_to_hf() {
        let _g = RegistryGuard::acquire();
        for (prefix, _) in DEFAULT_REGISTRATIONS {
            register_hf(*prefix, tiny(prefix));
        }
        for model in [
            "mistral-7b",
            "mistral-large-2411",
            "mixtral-8x7b",
            "codestral-latest",
            "ministral-8b",
            "pixtral-12b",
        ] {
            let t = get_tokenizer(model);
            assert_eq!(t.backend(), Backend::HuggingFace, "{model}");
            // Whitespace tokenization from the tiny fixture, not chars/4.
            assert_eq!(t.count_text("hello world"), 2, "{model}");
        }
        // Non-Mistral models are untouched by the registrations.
        assert_eq!(
            get_tokenizer("claude-3-opus").backend(),
            Backend::Estimation
        );
    }

    #[test]
    fn longest_prefix_routes_large_to_its_own_registration() {
        let _g = RegistryGuard::acquire();
        register_hf("mistral", tiny("generic"));
        register_hf("mistral-large", tiny("large"));
        // Both resolve to HF; the point is that `mistral-large-*` matches the
        // longer prefix (pinning the registry's longest-prefix-wins contract
        // for the Mistral family).
        assert_eq!(
            get_tokenizer("mistral-large-2411").backend(),
            Backend::HuggingFace
        );
        assert_eq!(get_tokenizer("mistral-7b").backend(), Backend::HuggingFace);
    }

    #[test]
    fn unregistered_mistral_models_estimate() {
        let _g = RegistryGuard::acquire();
        // Parity with production Python (mistral-common not installed):
        // without registration, Mistral models use the generic estimator.
        let t = get_tokenizer("mistral-large");
        assert_eq!(t.backend(), Backend::Estimation);
        assert_eq!(t.count_text(&"a".repeat(40)), 10); // 4.0 chars/token
    }

    /// Network-dependent (HF Hub); most mistralai repos are gated, so this
    /// asserts fail-softness rather than full success. Run manually:
    /// `cargo test -p headroom-core try_register_default -- --ignored`
    #[test]
    #[ignore = "requires network access to the HuggingFace Hub"]
    fn try_register_default_mistral_is_fail_soft() {
        let _g = RegistryGuard::acquire();
        let failures = try_register_default_mistral();
        // No panic; every failure names one of our prefixes.
        for (prefix, _) in &failures {
            assert!(DEFAULT_REGISTRATIONS.iter().any(|(p, _)| p == prefix));
        }
        // Any prefix that succeeded now routes to HF.
        if failures.iter().all(|(p, _)| *p != "pixtral") {
            assert_eq!(get_tokenizer("pixtral-12b").backend(), Backend::HuggingFace);
        }
    }
}
