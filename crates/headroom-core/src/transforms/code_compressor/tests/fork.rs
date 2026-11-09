//! Fork-only code compressor tests, moved out of `code_compressor.rs`.

use super::*;
use std::time::{Duration, Instant};

// ─── Language coercion (upstream addition) ───────────────────────────

#[test]
fn coerce_language_matches_python() {
    for (input, expected) in [
        ("js", CodeLanguage::Javascript),
        ("jsx", CodeLanguage::Javascript),
        ("mjs", CodeLanguage::Javascript),
        ("cjs", CodeLanguage::Javascript),
        ("node", CodeLanguage::Javascript),
        ("ts", CodeLanguage::Typescript),
        ("tsx", CodeLanguage::Typescript),
        ("py", CodeLanguage::Python),
        ("python3", CodeLanguage::Python),
        ("golang", CodeLanguage::Go),
        ("rs", CodeLanguage::Rust),
        ("c++", CodeLanguage::Cpp),
        ("cxx", CodeLanguage::Cpp),
        ("cc", CodeLanguage::Cpp),
        ("hpp", CodeLanguage::Cpp),
        ("pl", CodeLanguage::Perl),
        ("phtml", CodeLanguage::Php),
        ("php5", CodeLanguage::Php),
        ("php7", CodeLanguage::Php),
        ("php8", CodeLanguage::Php),
        // Canonical names still work, case-insensitively and trimmed.
        ("php", CodeLanguage::Php),
        ("python", CodeLanguage::Python),
        ("JavaScript", CodeLanguage::Javascript),
        ("  TS  ", CodeLanguage::Typescript),
        // Unknown yields Unknown rather than an error.
        ("", CodeLanguage::Unknown),
        ("zzz", CodeLanguage::Unknown),
        ("cobol", CodeLanguage::Unknown),
    ] {
        assert_eq!(CodeLanguage::coerce(input), expected, "coerce({input:?})");
    }
}

#[test]
fn an_unknown_hint_falls_back_to_content_detection() {
    // Previously an unrecognized hint resolved to Unknown, which skipped
    // code-aware compression for the whole block. It must now fall through
    // to detection — Python resolves this same input to `python`.
    let code = "def alpha():\n    return 1\n\ndef beta():\n    return 2\n".repeat(12);
    let cfg = CodeCompressorConfig {
        enable_ccr: false,
        fallback_to_kompress: false,
        language_hint: Some("cobol".to_string()),
        ..Default::default()
    };
    let r = CodeAwareCompressor::new(cfg).compress(&code);
    assert_eq!(r.language, CodeLanguage::Python, "should detect by content");
}

#[test]
fn an_alias_hint_is_honoured_without_detection() {
    // `py` must resolve directly, not fall through to detection. The input
    // has to clear `min_tokens_for_compression` (100) or the compressor
    // returns early with `Unknown` before any language resolution runs.
    let code = "def alpha():\n    return 1\n\ndef beta():\n    return 2\n".repeat(12);
    let cfg = CodeCompressorConfig {
        enable_ccr: false,
        fallback_to_kompress: false,
        language_hint: Some("py".to_string()),
        ..Default::default()
    };
    let r = CodeAwareCompressor::new(cfg).compress(&code);
    assert_eq!(r.language, CodeLanguage::Python);
    assert!(
        (r.language_confidence - 1.0).abs() < 1e-9,
        "explicit hint is certain"
    );
}

#[test]
fn leading_doc_comments_stay_attached_to_their_imports() {
    // Doc comments are top-level SIBLINGS of the import they document. Left
    // uncaptured they fall through to the leftover top-level sweep and get
    // grouped away from the import. Byte-for-byte what Python emits for
    // this input.
    let body: String = (0..24)
        .map(|i| format!("    step_{i} = compute(value_{i}, factor={i})\n"))
        .collect();
    let code = format!(
        "# Standard library imports for the parser.\nimport os\nimport sys\n\n\
             # Third-party: used by the tokenizer.\nimport regex\n\n\ndef alpha(x):\n{body}    return step_0\n"
    );
    let cfg = CodeCompressorConfig {
        enable_ccr: false,
        fallback_to_kompress: false,
        semantic_analysis: false,
        max_body_lines: 3,
        ..Default::default()
    };
    let r = CodeAwareCompressor::new(cfg).compress(&code);
    // Written as explicit lines: a `\` continuation would eat the leading
    // indentation that is part of the expected bytes.
    let expected = concat!(
        "# Standard library imports for the parser.\n",
        "import os\n",
        "import sys\n",
        "# Third-party: used by the tokenizer.\n",
        "import regex\n",
        "\n",
        "def alpha(x):\n",
        "    step_0 = compute(value_0, factor=0)\n",
        "    step_1 = compute(value_1, factor=1)\n",
        "    step_2 = compute(value_2, factor=2)\n",
        "    # [22 lines omitted]\n",
        "    pass",
    );
    assert_eq!(r.compressed, expected);
}

#[test]
fn validated_candidate_reverts_only_the_offending_node() {
    // The recovery pass exists so ONE bad candidate doesn't sink the whole
    // output. Splice a deliberately broken candidate in place of a function
    // and confirm the original text comes back; splice a valid one and
    // confirm it is kept.
    let code = "def alpha(x):\n    return x\n\ndef beta(y):\n    return y\n";
    let tree = parse_code(code, CodeLanguage::Python).expect("parses");
    let root = tree.root_node();
    let mut cursor = root.walk();
    let func = root
        .children(&mut cursor)
        .find(|c| c.kind() == "function_definition")
        .expect("has a function");

    let lang = lang_config(CodeLanguage::Python).expect("python config");
    let analysis = SymbolAnalysis::default();
    let body_limits = HashMap::new();
    let config = CodeCompressorConfig::default();
    let ctx = Ctx {
        code,
        code_lines: code.split('\n').collect(),
        language: CodeLanguage::Python,
        lang: &lang,
        body_limits: &body_limits,
        analysis: &analysis,
        config: &config,
        recover_invalid_python_nodes: true,
    };

    // Broken candidate -> module no longer parses -> original restored.
    let broken = "def alpha(x:\n    return".to_string();
    assert_eq!(
        ctx.validated_candidate(func, broken),
        "def alpha(x):\n    return x",
        "an unparseable candidate must revert to the node's original text"
    );

    // Valid candidate -> kept as-is.
    let ok = "def alpha(x):\n    pass".to_string();
    assert_eq!(ctx.validated_candidate(func, ok.clone()), ok);
}

#[test]
fn validated_candidate_is_inert_outside_the_recovery_pass() {
    // The first pass must not pay for per-candidate re-parsing; the
    // flag gates the path for every language, not just Python.
    let code = "def alpha(x):\n    return x\n";
    let tree = parse_code(code, CodeLanguage::Python).expect("parses");
    let root = tree.root_node();
    let mut cursor = root.walk();
    let func = root
        .children(&mut cursor)
        .find(|c| c.kind() == "function_definition")
        .expect("has a function");
    let lang = lang_config(CodeLanguage::Python).expect("python config");
    let analysis = SymbolAnalysis::default();
    let body_limits = HashMap::new();
    let config = CodeCompressorConfig::default();
    let ctx = Ctx {
        code,
        code_lines: code.split('\n').collect(),
        language: CodeLanguage::Python,
        lang: &lang,
        body_limits: &body_limits,
        analysis: &analysis,
        config: &config,
        recover_invalid_python_nodes: false,
    };
    let broken = "def alpha(x:".to_string();
    assert_eq!(
        ctx.validated_candidate(func, broken.clone()),
        broken,
        "flag off: candidate passes through untouched"
    );
}

#[test]
fn csharp_compresses_byte_identically_to_python() {
    let body: String = (0..20)
        .map(|i| format!("            var step{i} = Compute(value{i}, {i});\n"))
        .collect();
    let code = format!(
        "using System;\nusing System.Collections.Generic;\n\nnamespace Acme.Widgets\n{{\n    // A widget service.\n    public class WidgetService\n    {{\n        public int Process(int input)\n        {{\n{body}            return step0;\n        }}\n\n        public string Describe()\n        {{\n{body}            return \"widget\";\n        }}\n    }}\n}}\n"
    );
    let cfg = CodeCompressorConfig {
        enable_ccr: false,
        fallback_to_kompress: false,
        semantic_analysis: false,
        max_body_lines: 2,
        language_hint: Some("csharp".to_string()),
        ..Default::default()
    };
    let r = CodeAwareCompressor::new(cfg).compress(&code);
    assert_eq!(r.language, CodeLanguage::CSharp);
    assert!(r.syntax_valid, "compressed C# must re-parse");
    let expected = concat!(
        "using System;\n",
        "using System.Collections.Generic;\n",
        "\n",
        "namespace Acme.Widgets\n",
        "{\n",
        "    // A widget service.\n",
        "    public class WidgetService\n",
        "    {\n",
        "        public int Process(int input)\n",
        "        {\n",
        "            var step0 = Compute(value0, 0);\n",
        "            var step1 = Compute(value1, 1);\n",
        "            // [19 lines omitted]\n",
        "        }\n",
        "        public string Describe()\n",
        "        {\n",
        "            var step0 = Compute(value0, 0);\n",
        "            var step1 = Compute(value1, 1);\n",
        "            // [19 lines omitted]\n",
        "        }\n",
        "    }\n",
        "}",
    );
    assert_eq!(r.compressed, expected);
}

#[test]
fn csharp_is_detected_without_a_hint() {
    // The prefilter must recognize C# on its own, and must not steal Java
    // (both use `public class`, so the disambiguation matters).
    let csharp = "using System;\nusing System.Linq;\n\nnamespace Acme.Svc\n{\n    public class Widget\n    {\n        public int Id { get; set; }\n        public void Run() { }\n    }\n}\n";
    let (lang, conf) = detect_language(csharp);
    assert_eq!(lang, CodeLanguage::CSharp, "confidence {conf}");
    assert!(
        (conf - 1.0).abs() < 1e-9,
        "Python reports 1.000, got {conf}"
    );

    let java = "package com.acme;\n\nimport java.util.List;\n\npublic class Widget {\n    private int id;\n    public void run() { }\n}\n";
    let (jlang, jconf) = detect_language(java);
    assert_eq!(jlang, CodeLanguage::Java, "C# must not steal Java");
    assert!((jconf - 1.0).abs() < 1e-9, "got {jconf}");
}

/// A namespaced service class with two long method bodies — the shape the
/// PHP `LangConfig` is built around (statement-scoped namespace, `use`
/// imports, `declaration_list` class body, `compound_statement` bodies).
fn php_service_sample() -> String {
    let body: String = (0..20)
        .map(|i| format!("        $step{i} = $this->compute($value{i}, {i});\n"))
        .collect();
    format!(
        "<?php\n\nnamespace Acme\\Widgets;\n\nuse Acme\\Support\\Logger;\nuse Acme\\Support\\Cache;\n\nclass WidgetService\n{{\n    public function process(int $input): int\n    {{\n{body}        return $step0;\n    }}\n\n    public function describe(): string\n    {{\n{body}        return 'widget';\n    }}\n}}\n"
    )
}

#[test]
fn php_compresses_byte_identically_to_python() {
    let code = php_service_sample();
    let cfg = CodeCompressorConfig {
        enable_ccr: false,
        fallback_to_kompress: false,
        semantic_analysis: false,
        max_body_lines: 2,
        language_hint: Some("php".to_string()),
        ..Default::default()
    };
    let r = CodeAwareCompressor::new(cfg).compress(&code);
    assert_eq!(r.language, CodeLanguage::Php);
    assert!(r.syntax_valid, "compressed PHP must re-parse");
    // Recorded from the Python reference on this exact input. The `<?php`
    // tag keeps its place as header code; the blank line between
    // `namespace` and the `use` block does not survive, because the
    // namespace is hoisted through `package_node` and re-emitted
    // immediately ahead of the imports.
    let expected = concat!(
        "<?php\n",
        "\n",
        "namespace Acme\\Widgets;\n",
        "use Acme\\Support\\Logger;\n",
        "use Acme\\Support\\Cache;\n",
        "\n",
        "class WidgetService\n",
        "{\n",
        "    public function process(int $input): int\n",
        "    {\n",
        "        $step0 = $this->compute($value0, 0);\n",
        "        $step1 = $this->compute($value1, 1);\n",
        "        // [19 lines omitted]\n",
        "    }\n",
        "    public function describe(): string\n",
        "    {\n",
        "        $step0 = $this->compute($value0, 0);\n",
        "        $step1 = $this->compute($value1, 1);\n",
        "        // [19 lines omitted]\n",
        "    }\n",
        "}",
    );
    assert_eq!(r.compressed, expected);
}

#[test]
fn php_is_detected_without_a_hint() {
    // PHP's `$sigil` variables also match the Perl prefilter, so this is
    // the case that regressed to Unknown in Python before the `<?php`
    // disambiguation landed.
    let code = php_service_sample();
    let (lang, conf) = detect_language(&code);
    assert_eq!(lang, CodeLanguage::Php, "confidence {conf}");
    assert!(
        (conf - 1.0).abs() < 1e-9,
        "Python reports 1.000, got {conf}"
    );

    // And PHP must not steal Perl, whose sigils it shares.
    let perl = "package Acme::Widget;\n\nuse strict;\nuse warnings;\n\nsub run {\n    my ($self) = @_;\n    return $self->{id};\n}\n\n1;\n";
    let (plang, _) = detect_language(perl);
    assert_eq!(plang, CodeLanguage::Perl, "PHP must not steal Perl");

    // Sigils alone are not Perl: shell, PHP and prose `$vars` must not
    // nominate it, and a lone sigil-only hit goes to Unknown.
    for not_perl in [
        "echo $HOME built in $BUILD_DIR ok\n",
        "the price is $5 and $10 for @all of %them\n",
        "$sigil $other @decorator %hash\n",
    ] {
        let (lang, _) = detect_language(not_perl);
        assert_ne!(lang, CodeLanguage::Perl, "sigils misread: {not_perl:?}");
    }
}

#[test]
fn rust_beats_cpp_on_shared_paths() {
    // `::` nominates Cpp on Rust code; anchored Rust syntax overrules it.
    let rust = "use std::collections::HashMap;\n\nfn get(map: &HashMap<String, u32>) -> u32 {\n    map[\"k\"]\n}\n";
    let (lang, _) = detect_language(rust);
    assert_eq!(lang, CodeLanguage::Rust, "Rust lost to Cpp on its own code");

    // And the reverse: real C++ with no Rust markers still wins.
    let cpp = "#include <vector>\n\nint get(const std::vector<int>& v) {\n    return v[0];\n}\n";
    let (lang, _) = detect_language(cpp);
    assert_eq!(lang, CodeLanguage::Cpp, "C++ lost on its own code");
}

#[test]
fn php_falls_back_to_verbatim_on_malformed_input() {
    // Unparseable PHP must come back unchanged rather than half-elided.
    let code = "<?php\nclass Broken {\n    public function oops( {\n".repeat(8);
    let cfg = CodeCompressorConfig {
        enable_ccr: false,
        fallback_to_kompress: false,
        semantic_analysis: false,
        max_body_lines: 2,
        language_hint: Some("php".to_string()),
        ..Default::default()
    };
    let r = CodeAwareCompressor::new(cfg).compress(&code);
    assert_eq!(r.compressed.trim_end(), code.trim_end());
}

// ─── Bash passthrough (upstream ee902074 / ee6f9db2) ────────────────
//
// Shell control-flow words (`then`/`fi`, `do`/`done`, `esac`) are
// anonymous grammar tokens the generic AST reassembler cannot preserve,
// so recognized Bash is returned byte-for-byte. No tree-sitter Bash
// grammar is bundled: detection is regex-only and the passthrough
// reports valid, which is also the no-parser ordering the reference
// requires.

/// The control-flow sample from the upstream issue: two `then`/`fi`
/// pairs plus nested expansions.
fn bash_issue_sample() -> &'static str {
    "#!/usr/bin/env bash\n\
         set -euo pipefail\n\
         \n\
         if [ -x \"$(command -v /opt/bin/forgejo)\" ]\n\
         then\n\
         \texport GITEA_WORK_DIR=/git/forgejo\n\
         \texport FORGEJO_CUSTOM=/etc/forgejo\n\
         \talias forge=\"sudo -Eu git GITEA_CUSTOM=${FORGEJO_CUSTOM} GITEA_WORK_DIR=${GITEA_WORK_DIR} /opt/bin/forgejo\"\n\
         fi\n\
         \n\
         if [[ \"$( git config --global alias.pushall 2>/dev/null )\" != '!git remote | xargs -L1 git push --all' ]]\n\
         then\n\
         \tgit config --global --unset-all alias.pushall\n\
         git config --global --add alias.pushall '!git remote | xargs -L1 git push --all'\n\
         fi\n"
}

fn bash_compressor() -> CodeAwareCompressor {
    CodeAwareCompressor::new(CodeCompressorConfig {
        min_tokens_for_compression: 1,
        enable_ccr: false,
        fallback_to_kompress: false,
        ..Default::default()
    })
}

#[test]
fn bash_aliases_are_canonicalized() {
    assert_eq!(CodeLanguage::coerce("bash"), CodeLanguage::Bash);
    assert_eq!(CodeLanguage::coerce("shell"), CodeLanguage::Bash);
    assert_eq!(CodeLanguage::coerce("sh"), CodeLanguage::Bash);
    assert_eq!(CodeLanguage::coerce("zsh"), CodeLanguage::Bash);
    assert_eq!(CodeLanguage::coerce("shellscript"), CodeLanguage::Bash);
    assert_eq!(CodeLanguage::from_name("bash"), Some(CodeLanguage::Bash));
    assert_eq!(CodeLanguage::Bash.value(), "bash");
}

#[test]
fn bash_is_detected_from_control_flow() {
    let code = bash_issue_sample()
        .strip_prefix("#!/usr/bin/env bash\n")
        .unwrap();
    let (detected, confidence) = detect_language(code);
    assert_eq!(detected, CodeLanguage::Bash);
    assert!(confidence > 0.0);
}

#[test]
fn bash_is_detected_from_shebang() {
    let (detected, confidence) = detect_language("#!/bin/sh\necho hello\n");
    assert_eq!(detected, CodeLanguage::Bash);
    assert!(confidence > 0.0);
}

#[test]
fn bash_compression_is_lossless() {
    let code = bash_issue_sample();
    let r = bash_compressor().compress(code);
    assert_eq!(r.language, CodeLanguage::Bash);
    assert!(r.syntax_valid);
    assert_eq!(r.compressed, code);
    assert_eq!(r.compressed_tokens, r.original_tokens);
    assert_eq!(r.compression_ratio, 1.0);
}

#[test]
fn shell_hint_is_lossless() {
    let code = bash_issue_sample();
    let r = bash_compressor().compress_with(code, Some("shell"), "");
    assert_eq!(r.language, CodeLanguage::Bash);
    assert_eq!(r.compressed, code);
    assert!(r.syntax_valid);
}

#[test]
fn bash_control_flow_nodes_pass_through_byte_exact() {
    // One sample per opaque node class in the reference config:
    // if/for/while/case/subshell. Each must survive compression
    // byte-for-byte, delimiters included.
    let samples = [
        "if [ -f /etc/os-release ]; then\n\techo present\nfi\n",
        "for f in *.log; do\n\tgzip \"$f\"\ndone\n",
        "while read -r line; do\n\techo \"$line\"\ndone < input.txt\n",
        "case \"$1\" in\n\tstart) exec foo;;\n\tstop) exit 0;;\nesac\n",
        "#!/bin/bash\n(cd /tmp && tar czf out.tgz src)\n",
    ];
    for code in samples {
        let r = bash_compressor().compress_with(code, Some("bash"), "");
        assert_eq!(r.language, CodeLanguage::Bash, "for {code:?}");
        assert_eq!(r.compressed, code, "byte-exact for {code:?}");
        assert!(r.syntax_valid);
        assert_eq!(r.compression_ratio, 1.0);
    }
}

// ─── Syntax breaker (upstream dde83c91) ─────────────────────────────

/// Serialize breaker tests: the state is process-global and the runner
/// is multi-threaded.
fn breaker_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn reset_syntax_breaker() {
    let mut state = syntax_breaker_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    state.outcomes.clear();
    state.open_until.clear();
    state.trips.clear();
    state.invalid_input_skips.clear();
}

#[test]
fn breaker_trips_after_min_failures_in_window() {
    let _guard = breaker_test_lock();
    reset_syntax_breaker();
    for _ in 0..SYNTAX_BREAKER_MIN_FAILURES - 1 {
        record_syntax_outcome("typescript", false);
    }
    assert!(!syntax_breaker_open("typescript"));
    record_syntax_outcome("typescript", false);
    assert!(syntax_breaker_open("typescript"));
    // Per-language isolation: python keeps compressing.
    assert!(!syntax_breaker_open("python"));
    reset_syntax_breaker();
}

#[test]
fn breaker_successes_keep_it_closed() {
    let _guard = breaker_test_lock();
    reset_syntax_breaker();
    for _ in 0..SYNTAX_BREAKER_WINDOW * 2 {
        record_syntax_outcome("typescript", true);
    }
    for _ in 0..SYNTAX_BREAKER_MIN_FAILURES - 1 {
        record_syntax_outcome("typescript", false);
    }
    assert!(!syntax_breaker_open("typescript"));
    reset_syntax_breaker();
}

#[test]
fn breaker_reopens_after_cooldown() {
    let _guard = breaker_test_lock();
    reset_syntax_breaker();
    for _ in 0..SYNTAX_BREAKER_MIN_FAILURES {
        record_syntax_outcome("typescript", false);
    }
    assert!(syntax_breaker_open("typescript"));
    // Expire the cooldown directly (the clock itself is not patchable).
    {
        let mut state = syntax_breaker_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let past = Instant::now() - Duration::from_secs(1);
        state.open_until.insert("typescript".to_string(), past);
    }
    assert!(!syntax_breaker_open("typescript"));
    // The cleared window means one more failure does not instantly re-trip.
    record_syntax_outcome("typescript", false);
    assert!(!syntax_breaker_open("typescript"));
    reset_syntax_breaker();
}

#[test]
fn breaker_ignores_invalid_input_fragments() {
    // Fragments that never parsed are input noise, not compressor
    // failure: any number of skips must not trip the breaker.
    let _guard = breaker_test_lock();
    reset_syntax_breaker();
    for _ in 0..SYNTAX_BREAKER_WINDOW * 2 {
        record_syntax_input_skipped("rust");
    }
    assert!(
        !syntax_breaker_open("rust"),
        "skipped fragments must never trip the breaker"
    );
    // And failures on parseable input still trip afterwards: the skips
    // did not dilute the window.
    for _ in 0..SYNTAX_BREAKER_MIN_FAILURES {
        record_syntax_outcome("rust", false);
    }
    assert!(syntax_breaker_open("rust"));
    reset_syntax_breaker();
}

#[test]
fn validated_candidate_reverts_broken_candidate_in_rust() {
    // Per-node recovery is grammar-agnostic, not Python-only.
    let code = "fn alpha(x: i32) -> i32 {\n    x\n}\n";
    let tree = parse_code(code, CodeLanguage::Rust).expect("parses");
    let root = tree.root_node();
    let mut cursor = root.walk();
    let func = root
        .children(&mut cursor)
        .find(|c| c.kind() == "function_item")
        .expect("has a function");
    let lang = lang_config(CodeLanguage::Rust).expect("rust config");
    let analysis = SymbolAnalysis::default();
    let body_limits = HashMap::new();
    let config = CodeCompressorConfig::default();
    let ctx = Ctx {
        code,
        code_lines: code.split('\n').collect(),
        language: CodeLanguage::Rust,
        lang: &lang,
        body_limits: &body_limits,
        analysis: &analysis,
        config: &config,
        recover_invalid_python_nodes: true,
    };
    let broken = "fn alpha(x: i32) -> i32 {\n".to_string();
    assert_eq!(
        ctx.validated_candidate(func, broken),
        "fn alpha(x: i32) -> i32 {\n    x\n}",
        "an unparseable candidate must revert even outside Python"
    );
}

#[test]
fn breaker_env_kill_switch() {
    let _guard = breaker_test_lock();
    reset_syntax_breaker();
    for raw in ["0", "false", "off", "  Off "] {
        unsafe { std::env::set_var(SYNTAX_BREAKER_ENV, raw) };
        for _ in 0..SYNTAX_BREAKER_WINDOW {
            record_syntax_outcome("typescript", false);
        }
        assert!(
            !syntax_breaker_open("typescript"),
            "breaker must stay closed with {raw:?}"
        );
        reset_syntax_breaker();
    }
    unsafe { std::env::remove_var(SYNTAX_BREAKER_ENV) };
    // Sanity: without the opt-out the same run trips.
    for _ in 0..SYNTAX_BREAKER_MIN_FAILURES {
        record_syntax_outcome("typescript", false);
    }
    assert!(syntax_breaker_open("typescript"));
    reset_syntax_breaker();
    unsafe { std::env::remove_var(SYNTAX_BREAKER_ENV) };
}

#[test]
fn breaker_compress_short_circuits_while_open() {
    let _guard = breaker_test_lock();
    reset_syntax_breaker();
    // Long-bodied TypeScript functions: the AST pass would truncate the
    // bodies, so byte-identical output proves it never ran.
    let body: String = (0..20)
        .map(|i| format!("    const step{i} = compute(value{i}, {i});\n"))
        .collect();
    let code =
        format!("function alpha(x: number): number {{\n{body}    return step0;\n}}\n").repeat(6);
    assert!(estimate_tokens(&code) >= 100);
    {
        let mut state = syntax_breaker_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.open_until.insert(
            "typescript".to_string(),
            Instant::now() + Duration::from_secs(60),
        );
    }
    let cfg = CodeCompressorConfig {
        min_tokens_for_compression: 1,
        enable_ccr: false,
        fallback_to_kompress: false,
        language_hint: Some("typescript".to_string()),
        ..Default::default()
    };
    let r = CodeAwareCompressor::new(cfg).compress(&code);
    assert_eq!(r.language, CodeLanguage::Typescript);
    assert_eq!(r.compressed, code);
    assert_eq!(r.compression_ratio, 1.0);
    assert!(r.syntax_valid);
    reset_syntax_breaker();
}

#[test]
fn breaker_status_reports_open_languages_for_stats() {
    let _guard = breaker_test_lock();
    reset_syntax_breaker();
    assert!(syntax_breaker_status().is_empty());

    for _ in 0..SYNTAX_BREAKER_MIN_FAILURES {
        record_syntax_outcome("typescript", false);
    }
    let status = syntax_breaker_status();
    let ts = &status["typescript"];
    assert!(ts.open);
    assert_eq!(ts.trips, 1);
    assert!(
        (ts.reopens_in_seconds - SYNTAX_BREAKER_COOLDOWN_SECS as f64).abs() < 5.0,
        "got {}",
        ts.reopens_in_seconds
    );

    {
        let mut state = syntax_breaker_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let past = Instant::now() - Duration::from_secs(1);
        state.open_until.insert("typescript".to_string(), past);
    }
    let reopened = &syntax_breaker_status()["typescript"];
    assert!(!reopened.open);
    // The trip count survives the cooldown: a language that keeps
    // tripping is the field diagnosis, and it would be invisible if this
    // reset.
    assert_eq!(reopened.trips, 1);
    assert_eq!(reopened.reopens_in_seconds, 0.0);
    reset_syntax_breaker();
}

// ─── Upstream 7c150ec6b: Rust attributes stay with their items ───────

fn small_body_compressor(language: &str, max_body_lines: i64) -> CodeAwareCompressor {
    CodeAwareCompressor::new(CodeCompressorConfig {
        min_tokens_for_compression: 1,
        max_body_lines,
        enable_ccr: false,
        fallback_to_kompress: false,
        semantic_analysis: false,
        language_hint: Some(language.to_string()),
        ..Default::default()
    })
}

#[test]
fn rust_attributes_stay_attached_to_their_items() {
    let code = concat!(
        "#[derive(Debug, Clone, PartialEq)]\n",
        "pub struct Item {\n",
        "    pub id: u64,\n",
        "    pub name: String,\n",
        "}\n",
        "\n",
        "#[inline]\n",
        "#[cfg(feature = \"extra\")]\n",
        "pub fn log_item(item: &Item, sink: &mut Vec<u64>) {\n",
        "    sink.push(item.id);\n",
        "    sink.push(item.id + 1);\n",
        "    sink.push(item.id + 2);\n",
        "    sink.push(item.id + 3);\n",
        "}\n",
    );
    let r = small_body_compressor("rust", 1).compress(code);
    assert!(r.compression_ratio < 1.0, "{}", r.compressed);
    assert!(
        r.compressed
            .contains("#[derive(Debug, Clone, PartialEq)]\npub struct Item {"),
        "{}",
        r.compressed
    );
    assert!(
        r.compressed
            .contains("#[inline]\n#[cfg(feature = \"extra\")]\npub fn log_item"),
        "{}",
        r.compressed
    );
}

#[test]
fn rust_doc_comment_stays_ahead_of_attributes() {
    let code = concat!(
        "/// Records one item.\n",
        "#[inline]\n",
        "pub fn record(item: &Item, sink: &mut Vec<u64>) {\n",
        "    sink.push(item.id);\n",
        "    sink.push(item.id + 1);\n",
        "    sink.push(item.id + 2);\n",
        "}\n",
    );
    let r = small_body_compressor("rust", 1).compress(code);
    assert!(r.compression_ratio < 1.0, "{}", r.compressed);
    assert!(
        r.compressed
            .contains("/// Records one item.\n#[inline]\npub fn record"),
        "{}",
        r.compressed
    );
}

#[test]
fn rust_attribute_attaches_across_blank_line_and_detached_comment() {
    let across_blank = concat!(
        "pub struct Item { pub id: u64 }\n",
        "\n",
        "#[inline]\n",
        "\n",
        "pub fn record(item: &Item, sink: &mut Vec<u64>) {\n",
        "    sink.push(item.id);\n",
        "    sink.push(item.id + 1);\n",
        "    sink.push(item.id + 2);\n",
        "}\n",
    );
    let through_comment = concat!(
        "pub struct Item { pub id: u64 }\n",
        "\n",
        "#[inline]\n",
        "\n",
        "// Keep with the function below.\n",
        "\n",
        "pub fn record(item: &Item, sink: &mut Vec<u64>) {\n",
        "    sink.push(item.id);\n",
        "    sink.push(item.id + 1);\n",
        "    sink.push(item.id + 2);\n",
        "    sink.push(item.id + 3);\n",
        "}\n",
    );
    for code in [across_blank, through_comment] {
        let r = small_body_compressor("rust", 1).compress(code);
        assert!(r.compression_ratio < 1.0, "{}", r.compressed);
        assert!(r.syntax_valid);
        assert!(
            r.compressed.contains("#[inline]\npub fn record"),
            "{}",
            r.compressed
        );
        assert!(
            !r.compressed.trim_end().ends_with("#[inline]"),
            "{}",
            r.compressed
        );
    }
}

// ─── Upstream 0d0cda22b: multi-line class headers keep their brace ───

#[test]
fn opening_brace_line_survives_compression() {
    let rust = concat!(
        "impl<T> Wrapper<T>\n",
        "where T: Debug + Clone {\n",
        "    pub fn total(&self) -> u64 {\n",
        "        let mut acc = 0;\n",
        "        for item in &self.items {\n",
        "            acc += item.id;\n",
        "        }\n",
        "        acc\n",
        "    }\n",
        "}\n",
    );
    let java = concat!(
        "public class Calc\n",
        "    extends Base\n",
        "    implements Runnable {\n",
        "    public int compute(int x) {\n",
        "        int a = x + 1;\n",
        "        int b = a * 2;\n",
        "        int c = b - 3;\n",
        "        return c;\n",
        "    }\n",
        "}\n",
    );
    for (language, code, header) in [
        ("rust", rust, "where T: Debug + Clone {"),
        ("java", java, "implements Runnable {"),
    ] {
        let r = small_body_compressor(language, 2).compress(code);
        // A ratio of 1.0 means the syntax check rejected the output and the
        // compressor returned the original untouched.
        assert!(r.compression_ratio < 1.0, "{language}: {}", r.compressed);
        assert!(
            r.compressed.contains(header),
            "{language}: {}",
            r.compressed
        );
    }
}

// ─── Upstream 54f48fb1c: semicolon-separated statements ──────────────

#[test]
fn python_top_level_separator_does_not_become_a_bare_line() {
    let code = concat!(
        "import json\n",
        "B='/tmp/'\n",
        "v=json.load(open(B+'v.json')); m=json.load(open(B+'m.json'))\n",
        "print('oe', m['order_expire']); print('pay', m['pay_url'])\n",
    );
    let r = small_body_compressor("python", 5).compress(code);
    assert!(r.syntax_valid);
    assert_ne!(r.compressed, code);
    assert!(
        !r.compressed.lines().any(|l| l.trim() == ";"),
        "{}",
        r.compressed
    );
    assert!(r.compressed.contains("v=json.load(open(B+'v.json'))"));
    assert!(r.compressed.contains("m=json.load(open(B+'m.json'))"));
}

#[test]
fn python_class_body_statements_are_kept_once() {
    let r = small_body_compressor("python", 5).compress("class C:\n    x=1; y=2\n    z=3\n");
    assert!(r.syntax_valid);
    assert_eq!(
        r.compressed.matches("x=1; y=2").count(),
        1,
        "{}",
        r.compressed
    );

    let r = small_body_compressor("python", 5)
        .compress("class C:\n    x=1; y=(2 +\n            3)\n    z=4\n");
    assert!(r.syntax_valid);
    assert_eq!(
        r.compressed.matches("x=1; y=(2 +").count(),
        1,
        "{}",
        r.compressed
    );
    assert_eq!(
        r.compressed.matches("            3)").count(),
        1,
        "{}",
        r.compressed
    );
    assert_eq!(r.compressed.matches("z=4").count(), 1, "{}", r.compressed);
}

#[test]
fn python_import_after_statement_keeps_source_order() {
    let code = "import sys; sys.path.insert(0, '/tmp/vendor'); import plugin\n";
    let r = small_body_compressor("python", 5).compress(code);
    assert!(r.syntax_valid);
    assert_ne!(r.compressed, code);
    let at = |s: &str| {
        r.compressed
            .find(s)
            .unwrap_or_else(|| panic!("{s}: {}", r.compressed))
    };
    assert!(at("import sys") < at("sys.path.insert"), "{}", r.compressed);
    assert!(
        at("sys.path.insert") < at("import plugin"),
        "{}",
        r.compressed
    );
}

#[test]
fn python_function_body_keeps_each_line_once() {
    let code = concat!(
        "def f():\n",
        "    x=1; y=(2 +\n",
        "            3)\n",
        "    a=3\n",
        "    b=4\n",
        "    c=5\n",
        "    d=6\n",
        "    e=7\n",
        "    return x+y\n",
    );
    let r = small_body_compressor("python", 5).compress(code);
    assert_eq!(
        r.compressed.matches("x=1; y=(2 +").count(),
        1,
        "{}",
        r.compressed
    );
    assert_eq!(
        r.compressed.matches("            3)").count(),
        1,
        "{}",
        r.compressed
    );
}
