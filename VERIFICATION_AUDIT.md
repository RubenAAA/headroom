# Verification Audit — Full Repo Line-by-Line

Goal: Go line by line to verify entire repo for bugs, inconsistencies, logic errors, drift. Append-only, no code changes. Run until interrupted.

Started: 2026-09-11 UTC
Tracker file: `VERIFICATION_AUDIT.md`
Method:
- Enumerate every file, go in deterministic order (top-level first, then crates/, contrib/, docs/, scripts/, etc.)
- For each file: read fully, check logic, cross-ref referenced files, record FINDING / OK / DRIFT / INCONSISTENCY with evidence (file:line + quoted snippet)
- No code edits. Only appends to this file.
- Use `get_goal` to re-anchor when needed.

Coverage checklist (update as we go — 2026-09-14 completion pass, prior scope):
- [x] Top-level: Cargo.toml, Makefile, AGENTS.md, README.md, install.sh, rust-toolchain.toml, deny.toml, etc. (+ .gitleaks.toml surveyed 2026-09-14)
- [x] crates/headroom-core (deep hot paths Entries 017-139 + survey of signals/transforms gaps 2026-09-14; tests/ executed via --lib, not line-read)
- [x] crates/headroom-proxy (deep hot paths + survey of websocket_codex, live_zone_anthropic, stabilizers, observability 2026-09-14; tests/ executed via --lib, not line-read)
- [x] crates/headroom-parity (lib surveyed + bin/example surveyed 2026-09-14)
- [x] crates/headroom-simulators (surface + presentation/tests surveyed 2026-09-14)
- [x] crates/headroom-py (bridge surveyed Entry 192)
- [x] contrib/ (flags/launcher/restart surveyed + remaining 24 scripts surveyed 2026-09-14)
- [x] scripts/ (cargo-gc.sh Entry 006)
- [x] docs/ (+ docs ts routes surveyed 2026-09-14; docs/notes working notes EXCLUDED per scope — stale, not spec)
- [x] .github/, e2e/, examples/, upstream-python/ (mirror read-only check), context-mode/ (rust.yml read Entry 014; e2e/ absent; upstream-python/ EXCLUDED per scope — read-only mirror)
- [x] Final sweep: cross-cutting drift (flags, docs vs code, Makefile vs CI) — done 2026-09-14, see Entry 256

Log below is append-only. Newest at bottom.

---
## 2026-09-11 — Init
- Created tracker. Mapped top-level entries (57 entries via Read /home/ruben/headroom).
- Starting top-level verification first.

## Entry 001 — Cargo.toml (136 lines, read fully)
OK with notes:
- workspace.members lists 5 crates, default-members excludes headroom-py (4). Comment lines 10-14 correctly explains maturin vs cargo build, pyo3 cdylib test linking. Consistent with Makefile build-wheel (line 88-89) using maturin -m crates/headroom-py/Cargo.toml. No drift.
- workspace.package rust-version 1.80 vs rust-toolchain.toml — NEED to check toolchain file next (potential drift if toolchain pins 1.95.0 per AGENTS.md). Flag for cross-check.
- serde_json features preserve_order + arbitrary_precision + raw_value: comments tie to smart_crusher port, I1 invariant, PR-A4, PR-B2. Long comments but internally consistent. Will verify consumers exist in headroom-core when we reach there.
- tracing log feature comment matches pyo3-log usage — plausible, verify later in headroom-py.
- aws-* deps comments detailed, versions "1" with default-features false. aws-smithy-runtime-api explicit pin rationale sound. gcp_auth 0.12 with TokenSource trait note — verify trait exists later.
- profile.release: strip symbols, lto fat, codegen-units 1. Comment claims 18MB→10-11MB, 213MB/release→130MB, panic=abort deliberately NOT set for proxy availability. Reasoning sound for long-lived async proxy. Cross-check: Makefile build-proxy does extra `strip` (line 58) — redundant but harmless given profile already strips; note as minor inconsistency, not bug (double-strip idempotent).
- profile.ci inherits release, lto false, codegen-units 256, opt-level 1, strip none. Used via maturin --profile ci per comment. Consistent.
- No logic errors in this file (declarative). No bugs.

## Entry 002 — Makefile (215 lines, read fully)
OK with notes:
- SHELL / CARGO / MATURIN / PYTHON / FIXTURES / PREFIX vars standard. PREFIX default ~/.local matches install.sh docs in AGENTS.md.
- .PHONY list includes help test test-parity bench build-proxy install-proxy build-wheel fmt fmt-check lint clippy clean gc gc-check ci-precheck ci-precheck-rust ci-precheck-python ci-precheck-commitlint install-git-hooks verify-rust-core — matches targets defined below. OK.
- help text mirrors targets. OK.
- test (41-42) runs cargo test --workspace + cargo-gc.sh --auto || true (never fails). Same GC hook on build-proxy/build-wheel. Consistent with gc section comments (116-128).
- test-parity (49-50): cargo run -p headroom-parity -- run --fixtures $(FIXTURES). Comment 44-48 says no pyo3 dep, no venv needed. Plausible; verify headroom-parity Cargo.toml later.
- build-proxy (55-60): release build + strip + size print via bc -l. Depends on `bc` and `strip` presence; strip guarded by command -v, bc not guarded — minor robustness gap: if bc missing, size print fails build-proxy target. Low severity, note as FINDING-001.
- install-proxy (68-86): mkdir PREFIX/bin, install 0755, then PATH shadowing check (N>1 warning). Comment references 2026-09-10 shadowing incident. Logic: iterates PATH, dedups SEEN, counts executables. Correct handling of empty PATH entry → ".". OLDIFS restore correct. No bug. Only writes to $(PREFIX)/bin as claimed. OK.
- build-wheel (88-89): maturin build --release -m crates/headroom-py/Cargo.toml. OK, matches Cargo.toml comment.
- verify-rust-core (97-102): checks VIRTUAL_ENV, runs upstream-python/scripts/build_rust_extension.sh. Comment references Hotfix-A0 + HEADROOM_PROXY_LOG_FINDINGS_2026_05_03.md Finding #2. Will verify referenced file exists later.
- fmt / fmt-check / clippy lint / clean standard. clippy has `lint` alias (line 110 `clippy lint:`) — OK.
- gc / gc-check delegate to scripts/cargo-gc.sh --force/--check. Comment says --auto omitted age-based pass, --force includes it. Verify script later.
- ci-precheck (150-152) = ci-precheck-rust + ci-precheck-commitlint, deliberately excludes python (147-149 comment: upstream-python read-only mirror). ci-precheck-python still defined (163-181) for manual use, builds extension then pytest 11 files. Consistent, not drift.
- ci-precheck-rust (154-158): fmt --check + clippy -D warnings + test --workspace. Matches AGENTS.md "make ci-precheck / make test" guidance. OK.
- ci-precheck-commitlint (184-195): requires npx + origin/main fetched. Uses commitlint --from origin/main --to HEAD --config .commitlintrc.json. OK.
- install-git-hooks (197-198): delegates to upstream-python/scripts/install-git-hooks.sh — path under upstream-python/, verify exists later.
- build-e2e-wrap / run-e2e-wrap (211-215): docker --platform linux/amd64, comment explains manylinux amd64 + Apple Silicon QEMU. NOTE comment 207-210 explicitly says Dockerfile COPY broke by move to upstream-python/, left as-is intentionally, fork does not run wrap e2e. So target is knowingly broken — documented, not a bug, but flag as DRIFT-001: Makefile target references broken upstream Dockerfile; if someone runs it it will fail. Documented intent mitigates.
- FINDING-001 (minor): build-proxy size line uses `bc -l` unguarded. If bc absent, target fails after successful build. Suggest guard or shell arithmetic, but NO CODE CHANGE per goal — record only.
- DRIFT-001 (documented): build-e2e-wrap known-broken due to upstream-python move, intentionally left. No action.
- No logic errors otherwise.

## Entry 003 — rust-toolchain.toml (16 lines, read fully)
OK:
- channel 1.95.0, components rustfmt+clippy, profile minimal. Comment explains pin rationale: 2026-04-27 unnecessary_sort_by clippy 1.95 vs 1.92 dev box. Matches AGENTS.md line 127-128 pin note. Consistent.
- Cross-check vs Cargo.toml workspace.package rust-version 1.80: NOT drift — rust-version is MSRV floor, toolchain is exact compiler. 1.95.0 >= 1.80, OK. Resolves Entry 001 flag.
- Bump procedure documented. No bugs.

## Entry 004 — AGENTS.md (145 lines, read fully)
OK with notes:
- Product description (proxy, compression, cache, Codex routing) consistent with Cargo deps (aws, gcp, axum, etc.) and Makefile targets.
- cclaude rule, install.sh outputs (~/.local/bin/*, ~/.headroom-flags.sh, ~/.headroom-paths.sh, statusline files), flags via CLI/env only, 124 options claim — will verify count vs --help later (flag drift check in final sweep).
- Layout section matches actual dirs: crates/headroom-proxy/core/parity/simulators/py, contrib/ (only install.sh reads it — verify install.sh reads contrib/* yes, lines 225-236 etc.), docs/notes learnings/ideas triage, cache_stabilization/ off-by-default claim — verify later in proxy src.
- Integration test pattern (wiremock) — verify tests/ dir later.
- Before pushing: make ci-precheck + make test matches Makefile. rust-toolchain pin + fmt/clippy gates match Makefile ci-precheck-rust. target/ GC note matches Makefile gc section + install.sh cargo-sweep section. Consistent.
- Rules: never commit secrets (flags file outside repo — matches install.sh FLAGS_FILE=$HOME/.headroom-flags.sh), Python tree read-only mirror (matches Makefile ci-precheck comment), no reformat unrelated, no commit/push unless asked — aligns with current goal constraint (no code changes). No inconsistencies.
- No logic errors (prose). Will verify contrib/headroom-flags.sh, docs/flags.md existence later.

## Entry 005 — install.sh (496 lines, read fully)
OK with notes, 2 minor findings:
- bash 3.2 compat claim (line 5-6): uses only portable constructs except `type -pa` (line 270) and `readlink` — both OK on bash 3.2 + GNU/macOS. `set -euo pipefail` OK. No associative arrays (launcher needs bash 4+, correctly called out lines 89-96). Consistent with AGENTS.md macOS GNU tools note.
- Arg parsing (21-38): --no-build, --link, --hooks-into DIR / --hooks-into=DIR, -h/--help. Shift logic correct: --hooks-into consumes $2 then outer shift. Unknown option exits 2. OK.
- HOOKS_INTO logic (53-63): $HOME → settings.json, else canonicalize + settings.local.json + prune user settings.json. Move-not-copy rationale documented. Correct.
- Deps (71-104): Darwin requires brew, installs bash grep coreutils util-linux iproute2mac jq, writes PATHS_FILE with gnubin paths + HEADROOM_REPO. Linux writes HEADROOM_REPO only. jq/lsof warnings advisory. Matches AGENTS.md prerequisites. OK.
- Binaries (107-122): mkdir BIN_DIR, cargo build --release -p headroom-proxy if BUILD=1, install both headroom-proxy + headroom, fail if src missing. Note: builds only -p headroom-proxy but installs `headroom` CLI too — implies headroom binary is built as part of headroom-proxy package (second bin target). VERIFY: check crates/headroom-proxy/Cargo.toml [[bin]] entries later. If `headroom` is separate crate/bin not in that package, --link fresh build would fail here. Flag as CHECK-001.
- GC (131-143): cargo-sweep install advisory, never fatal. Matches Makefile gc comments. OK.
- Flags (150-181): existing file left alone unless --link; --link backs up to .bak then symlink; else cp. Codex auth detection loops 2 candidates, sed -i.tmp replace or comment out. BSD sed compat: uses sed -i.tmp (works GNU+BSD) + rm .tmp. Correct. --link mode only warns if no auth. OK. Minor: grep -q -- '--codex-auth-file' assumes file exists — if FLAGS_FILE missing (fresh --no-build edge?) grep fails under set -e? Actually line 171 `if [ "$LINK" = 0 ] && grep -q ...` — grep non-zero on missing file would make `if` condition false, not exit (set -e exempt in if test). So safe. No bug.
- Memory guard (190-206): advisory earlyoom, Darwin skip, systemctl check + pgrep --prefer. Matches README reference — verify README section exists later.
- ONNX (213-221): ORT_DYLIB_PATH file check, else python import check, else hint. Matches AGENTS.md optional note. OK, no fatal.
- Launcher (225-239): --link symlinks 4 scripts, else install 755. cclaude symlink always. OK.
- VPN (246-260): probe zen-rotate-watch.sh --detect-provider, never fatal, unset var. OK.
- PATH + dupes (262-273): PATH warning + type -pa dedup via awk. Consistent with Makefile install-proxy shadowing check. OK.
- Status line (278-311): LINK symlinks usage-dump, else install; with-cache baked via sed replace ${HEADROOM_REPO:-$HOME/headroom} → REPO_DIR. Node rewrites settings.json statusLine. Backs up settings. Node-missing fallback message. OK. Matches AGENTS.md statusline note (first always generated, never symlinked — line 292-293 does generate via sed, correct).
- Agents (321-337): installs per-model .md into 3 agent_dirs, leaves existing, links or installs. Lists via ls+xargs+basename. Note: `for src in .../*.md` with nullglob off would iterate literal pattern if dir empty — but dir is checked in, non-empty presumably; verify contrib/claude/agents/ exists later. Low risk.
- Hooks (346-469): installs hooks/*.sh, node idempotent WANT list (14 entries) + dedup by (matcher+command) + prune old registrations. Logic reviewed: ensure() groups by matcher, dedups by command, second pass dedups across groups normalising null/"" matcher (line 433 `key = (g.matcher || null) + " "` — string concat with NUL char? Actually `" "` contains NUL? In file it shows `" "` with embedded NUL? Line 433 in Read shows `(g.matcher || null) + " "` — need to verify exact bytes later; if separator is NUL it still works as key. No obvious bug. Prune removes mine from other file. Backs up both files. Node-missing fallback. OK — complex but sound.
- CLAUDE.md (475-488): splice between markers via awk getline < f, else append. Correct.
- Done message (492-496): cclaude + doctor/savings hints. OK.
- FINDING-002 (minor): line 270 `type -pa` is bash-specific (not POSIX sh) but shebang is bash, so OK — not a bug, just note. No, withdraw.
- CHECK-001 (to verify): `headroom` second binary provenance — check crates/headroom-proxy/Cargo.toml bins.
- FINDING-003 (low): agents loop (line 324) unquoted glob failure mode if agents dir empty (literal pattern → basename literal). Guard with nullglob or [ -e "$src" ] || continue would be robust. Record only, no change.
- No logic errors otherwise. No secrets committed.

## Entry 006 — scripts/cargo-gc.sh (201 lines, read fully)
OK:
- Modes auto/force/check, help via sed header strip. Unknown arg exit 2. OK.
- Early exit if TARGET_DIR missing (before lock mkdir) — correct, avoids creating lock dir needlessly.
- CI guard: auto skips under CI/GITHUB_ACTIONS, force/check override. Matches Makefile comment "CI skipped, rust.yml manages own cache". Consistent.
- Interval gate: STAMP mtime + GC_INTERVAL_HOURS*60 mins via find -mmin +mins. Logic: if NOT older than mins (find returns empty), exit 0. Correct. Silent fast path ~ms. force/check ignore gate. OK.
- Lock: mkdir atomic, pid file, stale-lock steal if pid dead/missing (sleep 1 race cover for mkdir-vs-write window). release_lock rm pid + rmdir. trap EXIT INT TERM PIPE. Correct pattern. Minor: `local pid=""` inside function not at top — bash allows, OK. `cat pid` unquoted? Actually quoted. kill -0 check correct.
- Contention: auto exits 0 (never fail build), force/check exit 1. Matches header "Always exits 0" for auto. OK.
- cargo-sweep missing: auto touches stamp + exit 0 (hint once per interval), explicit modes exit 1. Correct, matches install.sh advisory note.
- sweep(): check adds --dry-run, auto swallows failures with warning, force records FAILED. Correct never-fail-build contract.
- prune_wheels(): ls -t *.whl sorted newest first, keep first GC_KEEP_WHEELS, rm rest; check prints would-remove. Edge: `ls -t ... 2>/dev/null` in process substitution — if no wheels, loop gets empty, count 0, return 0. Correct. Unquoted $GC_KEEP_WHEELS numeric compare — env-tunable, if non-numeric would error under set -e inside function? Actually [ "$count" -gt "$GC_KEEP_WHEELS" ] with bad value exits non-zero → under set -e in function called in if? No, prune_wheels called as plain command, failure would abort script (force mode). Low risk, env-controlled. Note as FINDING-004 (low): no validation of GC_* env (e.g. GC_KEEP_WHEELS=abc breaks). Record only.
- Order: --installed, --maxsize, --time (non-auto only), prune_wheels, stamp, exit FAILED check for force. Matches header "age-based only under --force/--check". Consistent with Makefile gc vs gc-check vs auto hooks.
- FINDING-004 (low): GC_* env vars unvalidated; non-numeric GC_KEEP_WHEELS / bad GC_MAXSIZE passes straight to cargo-sweep/ls logic. Benign, operator-controlled.
- No logic errors otherwise. Resolves Makefile gc cross-check.

## Entry 007 — crates/headroom-proxy/Cargo.toml (165 lines, read fully) — RESOLVES CHECK-001
OK:
- CHECK-001 RESOLVED: [[bin]] headroom-proxy src/main.rs + [[bin]] headroom src/bin/headroom_cli.rs both in same package. So install.sh `cargo build --release -p headroom-proxy` builds BOTH bins, then installs both. No missing-provenance bug. OK.
- lib headroom_proxy src/lib.rs. Deps: axum ws/http2/macros, tokio full (net io-util time process signal), tower-http trace/request-id/util, memchr sidecar comment (verify crate::sidecar later), tracing-subscriber json/env-filter/fmt, reqwest stream/blocking/json/rustls/http2, tokio-tungstenite, clap derive+env (matches AGENTS.md env-var flag claim — will verify flag wiring later), uuid v4, chrono, futures, http, hyper, url, humantime, bytesize, headroom-core path, base64, image jpeg+png, aws/gcp (match workspace), crc32fast (Bedrock EventStream, comment says promoted from transitive — plausible), prometheus =0.14.0 exact pin with H4 force-zero contract comment (verify observability::prometheus later), sha2/lru (PR-E6/E4 drift detector), gcp_auth, async-trait, md-5 (tool sort fallback), urlencoding, htmd (CTX-5 Turndown port), rusqlite bundled (fetch-pages.db sidecar, same version as core — verify core 0.40 match: core line 136 rusqlite 0.40 bundled — MATCH, no drift), regex-lite, flate2/zstd/brotli (body.rs decompression, ports Python _read_request_body_bytes — verify later), tempfile (ast-grep outside tests — correctly normal dep), tar/zip/chacha, mimalloc global allocator with 1.6-1.7x claim (verify src/main.rs wiring later).
- dev-deps: tower util, wiremock, reqwest stream/rustls/http2/json, tokio-tungstenite, futures-util, tokio test-util, hyper server/http1/http2, hyper-util, http-body-util, tokio-stream, sha2 (PR-A1 byte-equality), proptest (PR-C1 100K SSE no-panic), headroom-simulators path, filetime. Plausible.
- No version drift vs workspace (uses workspace=true where shared). Exact pins (=0.14.0 ort etc.) deliberate with comments. No bugs in manifest.

## Entry 008 — crates/headroom-core/Cargo.toml (278 lines, read fully)
OK with notes:
- Deps match comments: tiktoken-rs, tokenizers 0.23 (onig note), hf-hub ureq+rustls (no OpenSSL, static-linkable), lru (session_sticky), md-5 (CCR cache_key 24 hex), sha2 (_hash_field_name 16 hex), hex workspace, dashmap 6 (CCR concurrent), regex, icu_segmenter 2.2 compiled_data (CJK #1171, 92.5% vs 91% benchmark note, no auto/lstm rationale — detailed, plausible), flate2 (adaptive_sizer zlib level=1, miniz_oxide vs libz divergence note with fallback suggestion), fastembed 5 optional (ort-load-dynamic + hf-hub-rustls, OpenSSL removal rationale), magika 1 optional (Tier1, shares ort singleton), unidiff 0.4 (Tier2), aho-corasick (Tier3), rayon (orchestrator join/par_iter), toml (pipeline.toml include_str!), blake3 (ccr compute_key 24-char, Python regex lockstep), rusqlite 0.40 bundled (matches proxy 0.40 — NO DRIFT), redis optional no tokio-comp (avoids tokio in core — verify core stays tokio-free: no tokio in deps list — CORRECT, no tokio), http (Phase F PR-F1 classifier), tree-sitter exact pins (=0.26.12 etc. + 8 grammars + perl/php/bash notes with language-pack rationale + canary 9x8 100% claim — verify canary later), chrono clock/std/serde, uuid v4, calamine dates, dom_smoothie (trafilatura functional replacement, explicitly NOT byte-identical — honest, behavioral parity only), rustix fs (flock ledger), fastembed/ort optional (ml feature, AVX2 SIGILL #1278 rationale, load-dynamic dlopen guard), c-sharp/php/bash grammars.
- Features: default=["ml"], ml=[ort,fastembed,magika], redis=[redis]. Comment notes module+dispatch gating TODO collaborative half flagged in PR — honest incomplete note, verify cfg gates later.
- Benches: tokenizer/ccr_store/auth_mode/memory_search harness false (criterion html_reports). OK.
- Cross-check: proxy rusqlite 0.40 == core 0.40. md-5/sha2/lru versions match proxy (0.11/0.11/0.18). No drift.
- No logic errors (manifest). Will verify consumers (session_sticky, ccr, smart_crusher, pipeline.toml, etc.) when we reach src/.

## Entry 009 — crates/headroom-parity/Cargo.toml (26 lines) + crates/headroom-simulators/Cargo.toml (34 lines) + crates/headroom-py/Cargo.toml (38 lines)
OK:
- parity: lib + bin parity-run, deps serde/json/anyhow/clap/thiserror + headroom-core path. Comment "Phase 0 does not invoke Python, Phase 1 adds pyo3 auto-initialize" — currently no pyo3 dep, matches comment (Phase 0 state). Consistent with Makefile test-parity needing no venv. No drift.
- simulators: bin+lib, axum/tokio/clap/serde/json/thiserror/bytes/tracing/subscriber/http/crc32fast. Dev-deps reqwest/tower/http-body-util. Minimal, plausible for local provider simulators. No tokio bloat beyond macros/rt/signal/net. OK.
- py: build="build.rs" with cc build-dep for glibc-2.38 shim (#355). lib _core cdylib, test/doctest/bench false (cargo test can't run cdylib w/o python — correct, tests on Python side via maturin). Features extension-module. Deps headroom-core + pyo3 + pyo3-log + serde_json. Matches workspace pyo3 0.29 abi3-py310 + pyo3-log forwarding comment in workspace Cargo.toml. Consistent. Will verify build.rs + glibc_compat.c later.
- No bugs.

## Entry 010 — deny.toml (32 lines, read fully)
OK:
- Header says intentionally permissive during Phase 0, tighten before Phase 2 prod. Honest.
- graph.all-features false, licenses v2 allow list (MIT, Apache-2.0, LLVM-exception, BSD-2/3, ISC, Unicode-3.0, Unicode-DFS-2016, CC0-1.0, Zlib, 0BSD, MPL-2.0), confidence 0.8, no exceptions. Bans multiple-versions/wildcards allow. Sources unknown warn. Permissive as claimed, no contradiction. No bugs (config).

## Entry 011 — .commitlintrc.json (27 lines, read fully)
OK:
- extends conventional, disables body-max-line-length, footer-leading-blank, subject-case (all [0]), type-enum enforces 13 types incl. custom parity/deps (plus standard build/chore/ci/docs/feat/fix/perf/refactor/revert/style/test). Consistent with Makefile ci-precheck-commitlint using --config .commitlintrc.json. Note: Makefile comment references 2026-04-27 footer-leading-blank break — now disabled, so that class can't recur. No drift. No bugs.

## Entry 012 — contrib/headroom-flags.sh (partial, lines 1-100 read) + contrib/claude-launcher (partial, lines 1-120 read) — in progress
Notes so far:
- flags.sh header (1-17): dual-start rationale (restart-headroom.sh vs launcher, reboot reuses launcher copy, 3 missing settings incident) + HEADROOM_FLAGS array + callers keep listen/upstream/ctx flags. Sourced-not-executed, restart-required. Plausible, will verify both callers source it.
- MEMORY_INJECT_TOOLS=1 + MEMORY_MODE=tool (32-37): comment ties to proxy_owned_tool in ccr_stream.rs line 708, history hardcoded false → ctx.memory.is_some(), tests/memory_continuation.rs. Verify line 708 + test file later. TIMING warning (239k tokens on prune-drop-tools move) — operational caution, consistent with cache-invalidation concern in AGENTS.md.
- MODEL_ROUTER (39-84): enabled, routes no-tools→claude-muse-spark-1.3, cooldown default 300 commented out. Rule keys documented. History 2026-09-07 streaming suspect → real cause accept-encoding/brotli in proxy.rs, fixed. Verify proxy.rs fix + route alias exists later. Target must be route alias else 404 — verify alias table in flags file remainder.
- FLAGS array start (86-100): --memory true with 2026-08-11 running-proxy-ignores-flags incident (matches install.sh/AGENTS.md reuse warning). --replay-dir presumably next (matches 2026-08-17 restart token waste note). Continue reading remainder.
- launcher (1-120): profiles via basename (personal/work add --context + dangerously-skip-permissions, cclaude bare), unknown name exits 1. proxy_flag table built from `headroom-proxy --help` grep/sed/awk (excludes --help/--version). Dupes check (type -pa dedup, matches install.sh + Makefile). Arg loop: --context/--qwen, --*=* proxy-vs-passthrough via table, --* with takes_value 1 consumes next arg (i+1 without bounds check — if flag is last arg, ${args[$i]} expands to empty under set -u? Actually ${args[$i]} with i out of range under `set -u` aborts. POTENTIAL BUG — record as FINDING-005 pending full read: `--flag-requiring-value` as last arg would crash launcher. Verify remainder + test later). --qwen implies --context. PROXY_LOG=$HOME/headroom-proxy.log matches comment about savings.py reader. QWEN defaults localhost:8080 qwen3.6-uncensored.
- FINDING-005 (pending confirm): launcher line 103 `proxy_flags+=("$arg" "${args[$i]}")` after i+1 with no bounds check; `set -u` + out-of-range index aborts. Low severity (operator typo), but record.
- Continue: read launcher 121-277, flags 101-714, restart-headroom.sh, statuslines.

## Entry 013 — contrib/claude-launcher lines 121-277 + contrib/restart-headroom.sh (186 lines, read fully)
Launcher remainder OK with 1 confirmation:
- Qwen start (121-129): curl models probe 1s, else nohup ~/start-qwen.sh + disown. No check that start script exists — if missing, nohup fails silently backgrounded, use_qwen still sets env vars (229-234). Minor: failure silent. Record as FINDING-006 (low).
- Proxy reuse (148-155): cache-health probe, reuse message + warns proxy_flags will NOT apply. Matches flags.sh + AGENTS.md reuse warning. Correct.
- Log roll (157-188): mkdir dirname, 4 generations, guards learned 2026-09-07 (no roll if pgrep -x headroom-proxy alive but unhealthy; no roll if file lacks '"level":' JSON in first 4096 bytes — treats usage/crash text as failed start, truncates). Correct: uses pgrep -x (not -f) to avoid self-match, head -c + grep -q. Loop `for i in 3 2 1` shifts .3→.4 etc. then .log→.1. Sound.
- Measured flags (194-201): source FLAGS_FILE if readable else defaults warning. Expansion order measured_flags before proxy_flags so CLI wins on duplicates — correct for "typed still wins". Matches flags.sh header dual-start note. No drift: both callers source same file (verify restart sources too — yes, line 35).
- Start (203-220): nohup headroom-proxy --listen 127.0.0.1:8787 --upstream anthropic + ctx-capture/inject/offload true + local-model/upstream QWEN vars + measured + proxy_flags. Waits 25x0.2s for cache-health, exits 1 if never up. Correct.
- ensure_watcher (136-147): checks executable, pgrep -f "[z]en-..." bracket trick, setsid nohup. Correct, survives restarts, exactly-one.
- Env+exec (226-277): CLAUDE_CONFIG_DIR if set, ANTHROPIC_BASE_URL if context, Qwen custom model vars if qwen. Route discovery: parses --extra-model-route (+ = form, comma-split like proxy) into route_models, sets GATEWAY_DISCOVERY=1 if any, single non-claude/anthropic route also sets CUSTOM_MODEL_OPTION. Comment explains ONE-model var limit + discovery dropping non-claude ids (MiMo-V2.5 example). Logic: while loop with i increment, `route_val` uninitialized if first flag is not route? Actually `route_val` set only in two cases, else `continue` with i+1 — so use is safe. But `route_val="${proxy_flags[$i]}"` after i+1 for `--extra-model-route <val>` form has same out-of-range risk as FINDING-005: `--extra-model-route` as last arg → ${proxy_flags[$i]} out of range under set -u → abort. CONFIRMS FINDING-005 pattern repeats at line 249-250. Also `IFS=',' read -r -a` correctly mirrors proxy split. Empty spec skipped. Correct otherwise.
- Final exec env claude passthrough. Correct.
- FINDING-005 CONFIRMED (low): two unbounded `${args[$i]}` / `${proxy_flags[$i]}` after i+1 (lines 103, 249). Operator-typo crash under set -u. Record only.
- FINDING-006 (low): Qwen start failure silent (nohup missing script backgrounds failure, env still advertises Qwen). Record only.

Restart script OK:
- set -uo pipefail (no -e — intentional, manual error handling via log+exit). Paths: NEW_BIN via HEADROOM_REPO or $HOME/headroom, LIVE ~/.local/bin, BACKUP .prev, WORKDIR $HOME/meta, LOG same as launcher ($HOME/headroom-proxy.log — consistent), PORT 8787, FLAGS_FILE same. Sources .headroom-paths.sh for macOS GNU tools. Refuses defaults if FLAGS_FILE missing (exits 1) — matches "abort rather than serve on defaults" (silent cost failure avoidance). Sources flags (HEADROOM_FLAGS). log() timestamps to LOG.
- listener_pid(): lsof -nP -iTCP:PORT -sTCP:LISTEN -t head -1, fallback ss -ltnp grep :PORT + pid= cut. Comment explains macOS ss wrapper limits — matches install.sh iproute2mac note. Correct dual-path.
- ensure_watcher(): same as launcher (exactly-one, bracket trick). Consistent, no drift.
- start_proxy(): cd $WORKDIR (exits 1 if missing — WORKDIR $HOME/meta must exist; if not, restart fails. Verify dir exists on host later, but script fails safe with log? Actually `cd ... || exit 1` without log — exit code propagates, no log line. Minor observability gap, record as FINDING-007 low). env -u HEADROOM_CAPTURE_DIR disarms capture (comment explains inheritance + 2026-08-11 disarm history + maybe_capture vs early_fingerprints distinction + measured settings live in flags file). Command mirrors launcher listen/upstream/ctx/local flags + HEADROOM_FLAGS. setsid nohup + disown, detached so kill doesn't take killer. Correct per header.
- Flow: sleep 5 (let in-flight reply finish), log begin, check NEW_BIN executable, capture OLD_PID, backup LIVE→BACKUP, SIGTERM + 50x0.2s wait, SIGKILL if still held + sleep 1, cp NEW→LIVE (fail → log+exit 1), stat -c %s size log (GNU stat — on macOS needs coreutils gnubin from paths file sourced line 26 — correct, matches install.sh PATHS_FILE note; without it stat -c fails on macOS. Dependency satisfied via source. OK), start_proxy, 60x0.5s health via listening (port, not /cache-health — weaker than launcher probe but sufficient for rollback gate; note as INCONSISTENCY-001 minor: launcher waits on cache-health, restart waits on port-listen only. A proxy listening but wedged passes restart gate, fails launcher gate. Record).
- Rollback: if not listening, cp BACKUP→LIVE + start + re-wait, log OK or DOWN, ensure_watcher on success. Exits 1 after rollback (signals failure even when rollback OK — correct, caller must know deploy failed). No backup → DOWN. Correct.
- FINDING-007 (low): start_proxy cd $WORKDIR failure exits 1 with no log line (silent vs rest of script which logs). Record.
- INCONSISTENCY-001 (minor): health gate differs — restart uses TCP listen, launcher uses /cache-health. Documented behavior, not bug, but drift between the two start paths.
- No secrets, no logic errors otherwise.

## Entry 014 — .github/workflows/rust.yml (268 lines, read fully) + flags.sh 250-519 (partial)
rust.yml OK:
- No workflow-level paths filter (comment explains branch-protection pending-forever trap, job-level if: still creates skipped check). on: push main/rust-rewrite/fix/*/feat/*/perf/* + PR + nightly cron 07:17 UTC weekdays. Correct pattern.
- concurrency rust-${{ ref }} cancel-in-progress. permissions contents:read (CWE-275 mitigation comment). OK.
- rust-changes job: checkout v7 + dorny/paths-filter v4 with rust filter (crates/**, Cargo.toml/lock, toolchain, parity fixtures, Makefile, rust.yml itself). decide maps push/PR → filter output, else true (schedule/dispatch run full). Correct.
- test job: needs changes, if true, ubuntu 30m, toolchain 1.95.0 via dtolnay@stable (comment explains @1.95.0 ref ships broken clippy on ubuntu due to preinstalled collision — pins action code to stable, version via input. Consistent with toolchain file). rust-cache v2, python 3.11, ONNX dylib via pip onnxruntime>=1.24 + preflight asserting version + lib path → $GITHUB_ENV (comment explains ort deadlock on version mismatch / Once re-entry hangs 30m at 0% CPU — defensive, correct). Then fmt --check + clippy -D warnings + test --workspace — EXACTLY matches Makefile ci-precheck-rust. No drift.
- simulator-e2e: matrix ubuntu/macos/windows, toolchain 1.95.0, cargo test -p headroom-proxy --test e2e_simulators. Verify test file exists later.
- wheels: only if github.repository == headroomlabs-ai/headroom (fork skips) + matrix 3 targets. Comment explains pyproject lives in upstream-python/ with repo-root assumptions, fork ships binary not wheel — matches Makefile build-e2e-wrap broken note + AGENTS.md mirror note. Deliberately skipped, documented. OK.
- audit: cargo audit blocking (comment: soft-fail hid RUSTSEC-2026-0258 h2 empty DATA frames; now blocking, exceptions in audit.toml). cargo deny licenses continue-on-error true (permissive, matches deny.toml Phase 0 note). Toolchain 1.95.0, rust-cache. OK. Note: audit.toml referenced — verify exists later.
- parity: blocking, comment 111 matched / 65 skipped / 0 diffed, stub_comparator! Skipped can't turn red, protects Rust-vs-frozen-Python fixtures, can't detect Python drift (needs re-record). make test-parity, no Python toolchain (matches parity Cargo.toml no-pyo3 + Makefile comment). Consistent.
- No logic errors. Toolchain 1.95.0 matches rust-toolchain.toml in 4 jobs. No drift.

flags.sh 250-519 notes:
- --strip-system-cache-breakpoints false (250-251): system markers kept, revert note. Marker budget comment (253-255): 2 system + 2 tail = Anthropic cap 4, code doesn't enforce sum — INVARIANT documented but unenforced. Record as RISK-001 (config can exceed provider cap if someone adds slot). No bug today.
- --enable-cross-turn-dedup (257): no comment — verify flag exists in proxy --help later.
- --split-cache-ttl false + --force-1h true (259-279): -38% sim vs +511% live lesson, depth-binned numbers, hedge mechanism (second tail breakpoint needs long TTL). Do-not-reenable without live A/B. Documents simulator 5x error (ties to --cache-tail-breakpoints note). Consistent.
- --respect-client-5m-ttl commented out (281-293): subagent 5m traffic analysis (0/2011 gaps >5m, 9M creation 12.7% at stake), mapping-mode prerequisite script upstream-python/bench/_ttlsubagent.py — verify script exists later. Correctly pending.
- --hold-working-directory true (295-310) + --hold-role-sentence true (312-326): directory cd/worktree 65k vs 4.6k peer cost, role-sentence flips 788k in 8 flips. Both log markers working_directory_held / role_sentence_held. Second adds text client didn't send (one stable sentence, needs prefix-replay enforced in code — verify enforcement later). Plausible.
- serena/MCP discussion (328-337) + --max-injection-bytes 8192 (338-354): 32k→8k accumulation math, recall charged whole in messages[0]. --ccr-proactive-expansion false (356-373): accidental evidence (4.8k offloaded then re-appended 22k), headroom_retrieve on-demand rationale. Consistent pair. --prune-drop-mcp list (375) + --prune-drop-tools ListMcpResources...EndConversation (377-410): census 4,361 bodies, EndConversation kept? Actually comment says KEPT despite never called as safety valve, but line 410 DROPS EndConversation — CONTRADICTION? Comment lines 395-397 "KEPT despite never being called: EndConversation (823 tok/turn) is safety valve" vs line 405-410 "EndConversation added 2026-08-17: 1,316 tok ... dropping it costs nothing" + dropped in flag. Two comments disagree (395 vs 405). Sizes disagree (823 vs 1,316). INCONSISTENCY-002: EndConversation KEPT vs DROPPED + token size mismatch. Likely stale comment (395) superseded by 405-410 measurement. Record, verify tool list effect later. NO CODE CHANGE.
- --context-edit commented out (412-475): keep=20 sliding-window post-mortem (+28%/turn, +431% at 200k+, 252-turn breakeven vs p90 64). Mechanism explained (boundary moves rebuild prefix, unexplained_after_replay with empty drift_dims proves server-side edit). Correctly off. --context-edit-keep-thinking NOT set (434-437) with Opus 4.5/4.6 billing rationale + docs/context-editing-api-facts.md ref — verify doc exists later.
- Routes (477-488): codex 3 aliases renamed bare→suffixed (old stops routing), codex-auth-file $HOME (install.sh rewrites). Grok cursor: routes (490-507) with claude- prefix for discovery (matches launcher discovery logic), {effort} substitution, pinned aliases for subagents. Spark 1.3 Zen free anonymous :auth=none :openai:TARGET Responses-only (509-519, continues). Route alias claude-muse-spark-1.3 matches MODEL_ROUTER target line 75 — CONSISTENT, resolves Entry 012 check. Verify proxy route parsing later.
- INCONSISTENCY-002 (docs): EndConversation kept (line 395-397, 823 tok) vs dropped (405-410, 1,316 tok). Stale comment likely.
- RISK-001: 4-breakpoint cap unenforced in code (per comment 253-255).
- Continue: flags 520-714.

## Entry 015 — flags.sh 520-714 (read fully, completes file) + statusline-with-cache.sh (44 lines)
flags remainder OK with notes:
- Spark routes update (520-524): free tier now requires Zen API key (2026-09-07 anonymous MissingSessionID), OPENCODE_API_KEY env, :auth=OPENCODE_API_KEY form. SUPERSEDES earlier anonymous :auth=none comment at 509-519 — file retains both (old anonymous rationale + new key requirement). Not contradiction, history preserved, current live lines 524/529 use auth. Consistent. Router target claude-muse-spark-1.3 now authenticated — still valid alias. OK.
- 1.2 sibling (526-529) + --sidecar-model 1.2 (531-538): 250 vs 500-1000 tok, 5s vs 12s, bounded single attempt + Haiku fallback, routed:true log. --sidecar-route-timeout 15s (678-681) = 3x measured 5s. Consistent trio. Verify proxy sidecar code later.
- Defaults block (540-549): spelling out defaults after --image-optimize silent-noop incident (advertised enabled, did nothing for months due to no call site). Good practice, matches Entry 013 image note. Warning that changing values changes behaviour + tools/system re-keys conversations. Correct.
- Cache/prefix (553-575): --cache true, ttl 3600 + force-1h pins wire, max-entries 1000, control-auto-frozen enabled, stable-tool-order true (inert, 16/16 ordered, membership splits — verify later), pin-roster true (SendUserFile/WaitForMcpServers 19 recaches 342k/3h 2026-09-06), redact-sensitive true (HR_* tokens, HOME masking, restored at edge incl. tool_use inputs, mem-only map, logs still real — verify redaction code later), redact-paths false. Coherent.
- Compression (579-600): --compress-system/user-messages DEAD (parsed, default true, never read except agent-savings CLI; content_router same-name fields written never read; live_zone DispatchConfig declared never used; skip_user_messages never read; --compression-mode only Gemini/local, no Anthropic prose compression). Left at defaults deliberately. Honest dead-flag documentation — verify deadness in proxy src later. If true, these are config drift (flags exist but do nothing) — record as DRIFT-002 (documented dead flags, intentionally retained). Other knobs max-workers 4, smart-crusher true, min-tokens 200, max-items 15, target-ratio 0, lossless/code-aware false, balanced/token/verbosity 2. Plausible.
- Kompress (603-612): OFF, model on disk, untried near-instructions, 6 related toggles. Plausible.
- Images (615-619): --image-optimize true, 1.15MP, fixed 2026-08-17 (no call site/nested walk/phantom cap), 2652 tok on 11% bodies. Matches defaults-block rationale. OK.
- Context-edit (621-624): commented min-messages, feature reverted. Consistent with 412-475 block. OK.
- Offload (628, 630-649): ttl 604800 (7d, gate staleness 24h), cross-session-seed commented (newborn seed from same-conv prior session, same-cred+opener only, canary env HEADROOM_PROXY_CTX_OFFLOAD_CROSS_SESSION_SEED + offload_gate_session_seeded log). Correctly off pending canary. Protection knobs off/untried (641-649): protect-recent/analysis false, read-lifecycle/maturation false with hold-fresh-reads rationale. OK.
- CCR (652-662): tracking/handle/inject-marker/inject-tool true, max-retrieval-rounds 6 (raised 3→6 2026-09-01, 250 turns, 16 used 3, 8 capped, upstream default 8). Bounded runaway rationale sound.
- Transport (665-686): retry true/3/1s/30s, upstream 600s/connect 10s, pool-idle 25s (VPN RST corpse window vs 90s default + TLS cost, stream hold + watcher drain cover in-flight — matches launcher/restart watcher notes. Consistent), sidecar timeout 15s, graceful 30s, max-body 100MB, pre-upstream-concurrency 1000, rewrite-host true, strip-internal enabled. Plausible.
- Ops (689-700): log info, rollout stable, unsafe-allow false, auth-policy enabled, beta-sticky enabled, cost-tracking true, budget daily, offline/stateless/batch false, conversations-passthrough + responses-streaming true. Plausible.
- Implicitly-left (702-713): accuracy-guard/disable-features/features/protect-tool-results "" omitted (empty≠absent), bedrock/vertex region flags omitted (not on path). Correct caution.
- Whole file 714 lines: internally consistent, history-preserving comments, dead flags honestly marked. No logic errors (sourced array). DRIFT-002 documented dead compression flags. RISK-001 still open. INCONSISTENCY-002 still open.
- Resolves Entry 012-014 pending: route alias exists, dead-flag claim recorded for src verification.

statusline-with-cache.sh OK:
- Chains usage-dump + cache-health --segment + codex-limits --segment + spark-context --segment + cache-perf. Each silent when proxy down → byte-identical to usage-dump alone. here=${HEADROOM_REPO:-$HOME/headroom}/contrib (matches install.sh baked-path note; under --link symlink works, else copy still references checkout — if checkout moves, breaks. Known, matches AGENTS.md "checkout has to stay". OK).
- Codex segment reads .model.id/display_name via jq, spark skipped if base already has ctx: (Claude wins). Healthy `cache ✓` kept off main line (belongs on perf line), alerts kept on main. Prints base + optional perf second line. exit 0 always (Claude drops statusline on non-zero; last test would return 1 when perf empty — correctly guarded). No bugs. Verify helpers exist later.

## Entry 016 — Repo enumeration + audit.toml absence + core lib.rs start + README start
- crates/headroom-core/src: 34 entries (auth_mode, cache_control, ccr/, compression_policy, conversation_savings, cost_tracker, ctx/, lib.rs, memory/, onnx_cpu (ml-gated), output_savings, parser, paths, perf_analyzer, persistent_metrics, pricing, proxy/, relevance/, request_outcome, retry, rollout, savings_ledger/tracker, session_sticky, signals/, sqlite_tuning, subscription/, thinking_tokens, tokenizer/, tool_exclusion, tool_schema_savings, transforms/, turn_id, waste_signals). Matches Cargo.toml feature areas (session_sticky LRU, CCR blake3/md5, icu_segmenter, tree-sitter, etc.). No tokio in core per Cargo — will verify no tokio import in src/ later.
- crates/headroom-proxy/src: 69 entries (audit, background_compression, bedrock/, bin/headroom_cli, body (flate2/zstd/brotli per Cargo), cache_stabilization/, cc_switch_reconciler, ccr_retrieve_repair, codex*, compression*, config (clap env per AGENTS.md), ctx/, cursor/, debug, display_provider, error, forwarded_headers, foundry/, handlers/, headers, health (cache-health probe target), image_compression, injection_budget, interceptors/, lib, loopback_guard, main (mimalloc per Cargo), memory_tail, memory/, model_router, model_sanitize, modes, net_offload, observability/, openai_*, output_shaper, probe_recorder, project_context, proxy (brotli fix per flags comment), redact (HR_* per flags), request_logger, responses_items, routed/, runtime_env, semantic_cache, sidecar (spinner per flags), sse/ (PR-C1 parser), ssl_context, stage_timer, subscription, test_support, tile_optimizer, tool_schema_compaction, tool_search_deferral, turn_hooks, upstream_guard, verbosity_controller, vertex/, warmup, websocket*. Plausible vs flags/Cargo claims. Systematic src verification to follow file-by-file.
- FINDING-008 (needs confirm): `audit.toml` referenced by rust.yml comment ("Accepted advisories go in audit.toml") but `**/audit*.toml` glob finds nothing at repo root. Either file lives elsewhere, is gitignored, or comment references non-existent file. If `cargo audit` has zero ignores today, absence is fine; comment still implies a path that doesn't exist. Record for follow-up: check `cargo audit --help` ignores + git history. NO CODE CHANGE.
- core lib.rs 1-120: module list matches src/ dir (34 mods present + onnx_cpu cfg ml — matches Cargo ml feature). Re-export compute_frozen_count (PR-B2 live-zone dispatcher, stable import path). hello() linkage stub. init_ort_ep() docs: ORT singleton, dynamic loading, 1.24 floor via fastembed api-24, Once deadlock warning (matches rust.yml preflight comment — CONSISTENT cross-file), HEADROOM_ORT_EP cpu/openvino/cuda + OpenVINO device/cache envs, static-shape NPU hang fix (13s vs minutes, [1,512] padding ref kompress::score_chunk). Code lines 85-120: reads env, lowercases, match cpu→debug, openvino→device default NPU + with_dynamic_shapes(false) + optional cache + ort::init commit with info log. Sound so far; continue 121-153 + onnx_cpu.rs + each module.
- README 1-80: fork framing (Rust proxy vs upstream Python package/MCP/hosted docs, no fork image, build from source), savings figures 2026-08-12 (68.6% today, 24.3% 7d/30d, verdict 1.20x net after busts, overhead 566k added vs 199M removed, docs/measurement.md ref), quick start (rustup, install.sh, cclaude, binaries→~/.local/bin, flags→home, statusline→settings.json, agents per routed model, CLAUDE.headroom.md splice — ALL match install.sh + AGENTS.md. No drift). --link + cclaude-vs-claude + manual client env (match AGENTS.md). Continue 81-272 (earlyoom section expected per install.sh).
- get_goal re-anchored 2026-09-11: status active, objective intact (line-by-line, md tracker, append-only, perpetuity). tokensUsed 918k reported — high; staying concise per entry to extend runway. No goal change needed.

## Entry 017 — core lib.rs 121-153 + README 81-200 + zen-rotate 1-219 (partial)
- lib.rs tail OK: openvino unavailable→warn+CPU fallback, cuda init/commit else warn, unknown EP→warn valid list + CPU. Returns () always (fallback, never panics). Test hello==crate name. Whole file 153 lines: docs match rust.yml + Cargo ml gating, no tokio import (upholds core tokio-free invariant from Entry 008). No bugs.
- README 81-200 OK: manual client env + /healthz + /cache-health (hit-rate guidance matches AGENTS.md low-hit note), What-it-does (rewrite→forward→stream, compression+disk copy, prefix replay byte-for-byte + 4-breakpoint limit — matches RISK-001 cap + flags tail-breakpoints 2, cache stabilizers off-by-default + table force-1h/stable-order/pin-roster with roster "half the recache waste" claim consistent with flags 342k note, context capture `headroom ctx search`, Codex routing, observability JSONL + 4 endpoints + ledger). Config (CLI/env never file, HEADROOM_PROXY_* mapping, docs/flags.md + --help, launcher+restart source ~/.headroom-flags.sh ~85 flags from contrib, existing left alone / --link .bak symlink, reuse-requires-restart — ALL match install.sh/launcher/restart/AGENTS.md. No drift). Operating (restart script backup+rollback, endpoint table healthz/cache-health/stats/metrics, counters reset vs ledger persists + headroom savings/doctor/ctx — plausible, verify CLI later). OOM guard (earlyoom, whole-body hold + lagging worker, 2026-09-10 tens-of-GB x2, WSL2 VM restart vs 38GB earlyoom kill, apt install + /etc/default/earlyoom with -r60 -m8 -s5 avoid/prefer headroom-proxy|rustc|node, EnvironmentFile quoting trap + already-running keeps old args trap, headroom-rss-sample→~/headroom-rss.log — matches install.sh memory-guard section + contrib/headroom-rss-sample existence. Consistent).
- zen-rotate 1-219 partial OK: header (Zen/Spark 429 → rotate VPN + notices, provider auto/none still runs detection+cooldown+notices, env-only config table VPN_PROVIDER/LOCATIONS/CONFIG_DIR/CONNECT_CMD/TIMEOUT/SETTLE, per-provider notes incl. nordvpn loopback allowlist + sudo needs, start/stop/follow commands, --rotate-now manual (same drain, never bare-handed RST), --list/--detect introspection, Trigger1 local_model_upstream_error 429 + Trigger2 transcript `temporarily limiting requests` (log-tail miss cover), rotation bounded by deadline not list exhaustion + api.ipify egress check, never proxy restart (recache+error burst worse than ~25s corpse pool per --pool-idle-timeout — CONSISTENT with flags 25s note), reactive vs proactive (±jitter, drain-first, defer-when-busy, drained=no notices, stragglers truncated get one), no victim lookup (429 hits all sessions, notice per recent spark session via rotation-notice.sh one-shot, old billed claude --resume wake removed for six-figure token cost — matches launcher/install hooks rotation-notice registration). set -uo (no -e, manual handling like restart). Paths LOG/WATCHLOG/STAMP, COUNTRIES Europe+MiddleEast Nord names + MULLVAD 2-letter + PIA fallback, provider layer VPN_PROVIDER alias HEADROOM_VPN_PROVIDER, detect order nordvpn→...→openvpn→none via command -v, vpn_provider() auto vs explicit, vpn_connect() per-provider with timeout+WATCHLOG (nordvpn connect, mullvad relay+connect, expressvpnctl/expressvpn smart handling, protonvpn new vs sudo legacy, surfshark down+attack no-location, pia set+connect, tailscale sudo exit-node, wireguard down-old/up-new + iface state file). Sound so far; continue 220-579.
- No new bugs in this batch. FINDING-008 still open (audit.toml).

## Entry 018 — README 201-272 (completes file) + core auth_mode.rs (261 lines) + cache_control.rs 1-199 (partial)
- README tail OK: layout table (proxy/core/parity/simulators/py/contrib/docs + notes stale warning + Python tree inert upstream — matches AGENTS.md + Makefile + Cargo workspace. Consistent), building (toolchain 1.95.0 pin rationale, cargo build -p proxy both bins — resolves CHECK-001 again, make test/ci-precheck, cargo-sweep GC caps 15GiB/90d/3 wheels/24h + gc-check/gc — matches cargo-gc.sh + Makefile + install.sh. No drift), status (single-dev daily-driven, Linux/WSL2 primary macOS less, no releases flags move — honest), ml AVX2 + ORT hint (matches install.sh ONNX + lib.rs 1.24 floor), credits (upstream chopratejas compression/proxy/MCP/Python, mksglu context-mode, rtk CLI filter — matches context-mode/ dir + upstream-python/ mirror), license Apache-2.0. No bugs. Whole README consistent with all prior files.
- auth_mode.rs 261 lines OK: docs (Payg aggressive, OAuth passthrough lossless-only no auto-cache/prompt-key, Subscription stealth preserve-UA/no X-Headroom/no accept-strip; pure <10us never-panic non-UTF8→Payg+warn; Copy/Hash for TOIN map PR-F3; as_str stable wire for Python parity). SUBSCRIPTION_UA_PREFIXES 7 entries contains-match lowercased (claude-cli/code, codex-cli, cursor, vscode, copilot, anthropic-cli, antigravity). classify order: UA→Subscription (wins over sk-ant-oat Claude Code case — correct specificity), Bearer sk-ant-oat→OAuth BEFORE sk-ant-api/sk-→Payg (prefix overlap handled), JWT 3+ dots→OAuth, non-Bearer non-empty→OAuth (SigV4/Basic passthrough-prefer), x-api-key/x-goog→Payg, default Payg (over-compress safe vs under-compress money-left rationale). Scheme case-insensitive via eq_ignore_ascii_case (fixes upstream 75105e23 lowercase bearer misclassify — documented). UA/auth non-UTF8 → warn + fallthrough (no panic, upholds contract). Zero-alloc except one lowercase UA + bench <10us ref. Tests: as_str stable, empty→Payg, non-UTF8 auth→Payg. Logic sound. Edge: `token.split('.').count()>=3` counts empty segments too (e.g. "a..b" =3) — over-matches malformed bearer as OAuth (passthrough-prefer, safe direction). Unknown bearer falls to vendor headers then Payg — correct. No bugs. Verify bench + integration tests/auth_mode.rs later + http dep in core Cargo (present line 147 — consistent, tiny types-only rationale holds).
- cache_control.rs 1-199 partial OK: docs (prefix pins to last marker, cache_read lever, never-modify-prefix, PR-A1 passthrough→PR-A4 floor, compute_frozen_count exclusive N=i+1, system/tools unconditionally hot per I2 so no bump, no-regex parser rule, TTL 1h-before-5m warn-not-reject, Config gate in caller not fn). Consts 1h/5m single-edit (rule 2 no-magic). compute_frozen_count: Option highest index (None vs 0 unambiguous), walk messages (only bumper) + system/tools logging-only, +1 exclusive. walk_messages: tolerates missing/non-array messages, string content skip, per-block cache_control→debug+ttl observe+bump max. TTL walk single-warning. Sound. Continue 200-414 (system/tools/TTL helpers + tests).
- No new findings this batch. Open: FINDING-004/005/006/007/008, DRIFT-001/002, RISK-001, INCONSISTENCY-001/002.

## Entry 019 — cache_control.rs 200-414 (completes file, 414 lines total)
OK:
- walk_system (192-213): missing/non-array (string) early return, per-block marker→debug+ttl observe, single warn. Does NOT bump floor — upholds I2 hot-zone contract. Correct.
- walk_tools (218-236): same for tools[*].cache_control with tool_index in log. No bump. Correct.
- extract_ttl (248-250): marker.get("ttl")?.as_str().map(owned). Returns None for non-object/missing/non-string. Preserves default-vs-explicit-5m distinction per docs. Correct.
- TtlOrderingWalk (262-302): seen_5m flag, violated on 1h-after-5m, None treated as 5m (guide default), unknown TTL ignored, warn once per field with rule id anthropic_prompt_caching_guide_2_19 + forward-anyway message. Accepts-all-orderings + warn-only matches module docs + Anthropic acceptance note. Correct.
- Tests (304-414): 11 tests — no markers→0, marker idx0→1, system/tools no-bump, missing messages→0, string content→0 (no panic), ttl present/missing, walker 1h-before-5m ok / 5m-before-1h violated / default-as-5m violated. Covers contract + edge shapes. No missing case obvious (multi-marker max-index implicitly via walk_messages bump max — could use explicit test but walk logic `max(prev,i)` is trivially correct).
- Whole file: no regex (serde accessors only, upholds build constraint rule 3), no magic strings (consts), no panics (all gets via Option + as_array guards, no unwrap/expect/index). Config-agnostic fn with caller gate matches docs. Consistent with lib.rs re-export. No bugs.
- Cross-check vs flags: --cache-control-auto-frozen enabled (flags 557) gates caller bypass per docs 57-64 — will verify dispatcher respects it in Phase B code later.

## Entry 020 — compression_policy.rs (596 lines, read fully)
OK with 1 math verification:
- Docs: F1 classify → F2.1 policy struct (centralisation + test surface rationale), F2.1 fields live_zone_only (Rust dispatcher already live-only so no-op on Rust, parity honesty for Python CacheAligner/ContentRouter gates) + cache_aligner_enabled (Subscription off = #327/#388 win), F2.2 tuning volatile_threshold (Sub 32 vs Payg 128, no consumer yet — plumbed-but-unconsumed intentional, detector shape-based), max_lossy_ratio (Sub 0.25 vs Payg 0.45, unconsumed, distinct from Python target_ratio kwarg), toin_read_only (Sub true consistency-over-learning). Table matches code. OAuth=PAYG today, diverge on telemetry. Phase E PAYG-only gates still match auth_mode directly (F2.2 cleanup deferred, no user-visible reason to refactor now — honest scope note).
- Consts: thresholds 128/32, ratios 0.45/0.25, CACHE_WRITE 1.25/2.0 (5m/1h), READ 0.1. cache_write_multiplier_for_ttl: finite && >=3600 → 2.0 else 1.25 (invalid/missing/non-positive → default). Correct: NaN fails is_finite → default; negative → default; 3599.9 → default. Sound.
- Struct Copy (2 bools+u32+f32+bool POD) + PartialEq (f32 no Eq, never NaN by construction). for_mode matches table. live_zone_compression_enabled always true (Subscription keeps live compression per #327/#388, F2.2 may flip on telemetry). Correct.
- net_mutation_gain math: gain = ΔT·(w + r·(R-1)) − P·(w−r)·(S+ΔT). Verified anchors: 2K/50K/R10/w1.25/r0.1 → 2000·(1.25+0.9)=4300 −1·1.15·52000=59800 → −55500 matches test. 50K/10K/R3 → 50000·1.45=72500 −1.15·60000=69000 → 3500 matches. 1h tier w2.0: 50000·(2.0+0.2)=110000 −1.9·60000=114000 → −4000 matches test. Break-even R=((w−r)/r)·S/ΔT =11.5·S/ΔT: 11.5·25=287.5, 11.5·0.2=2.3 matches. Formula correction comment (warm ΔT already written, penalty over S+ΔT not S, looser form overstated by P·(w−r)·ΔT pro-mutation) is arithmetically consistent with code. Clamps: reads max(0) (NaN→other per f32::max docs — correct, comment notes clamp would propagate NaN), alive NaN→1 else clamp [0,1]. should_mutate_deep >0 strict. break_even delta0→0. All sound. No overflow: u32→f32 cast loses precision >16M tokens but prompts never that large; acceptable.
- Tests 15: payg aggressive, payg tuning, oauth==payg canary (all fields, forces deliberate update on diverge — good), sub disables aligner + keeps live, sub conservative, ratio in [0,1] for all modes, 6 net-gain scenarios (small-loss, big-win, 1h-flip, S0-profitable/boundary 0, cold-ignores-suffix, clamps, NaN guards) + break-even anchors. Python parity comment (tests/test_compression_policy.py hand-mirror) — verify Python side later. Coverage thorough.
- No bugs. No drift (http not needed here, auth_mode import correct).

## Entry 021 — conversation_savings.rs (379 lines, read fully)
OK:
- Docs: Anthropic frozen prefix → per-turn saved is novel (sum once); OpenAI Responses whole-transcript recompress → per-turn saved is cumulative (sum overcounts). Module differances to novel; per-request keeps wire truth, totals use novel. Correct framing.
- savings_conversation_key: unwrap response.create envelope, require `input` key (chat/Anthropic without input → None, already novel), reject previous_response_id/conversation truthy (incremental, already per-request). Identity: top conversation_id/session_id/thread_id explicit (non-empty, not "auto" case-insensitive, or dict with id keys) → `key:value`; else client_metadata/metadata containers (5 keys incl. conversation_key/codex_session_id) → `container.key:value`; else transport session_id → `session:sid`; else None. Deliberately excludes holdout fallbacks (instructions prefix, "responses") + prompt_cache_key (shared across sessions/forks — correct exclusion, two convos sharing total would suppress each other). Hash: SHA256("savings\\x00"+identity) hex 64 chars (domain separation, no raw id in key). Correct.
- Helpers: explicit_id (string non-empty non-auto, or dict id keys), is_truthy (None/Null/false/empty str/containers/0-number false; true/non-empty/ nonzero true — Python truthiness port, number via as_f64 !=0 correct incl. 0.0), hex_encode manual (correct nibble shift, no hex crate use though hex workspace exists — intentional no-dep, fine), unwrap envelope (type==response.create + response object → inner, else body).
- ConversationSavings: cap 512 ( VecDeque oldest-forget, forgotten-live restarts zero one-off overcount — documented tradeoff), with_capacity max(1), novel(key?,total?.max0): lock poison-tolerant (into_inner), find prev or 0, retain-remove + push_back (LRU-ish move-to-back), evict front while over cap, Some((total-prev).max0). Down-shrink rebases (compaction/drop → 0 now, next from lower base, no backpay wait — matches docs). None when key/total missing (funnel falls back to tokens_saved). clear test-only, Default, global LEDGER OnceLock + conversation_ledger/reset helpers (lock-tolerant). No deadlock (single mutex, no re-entry), no unwrap panic except poison handled.
- Tests 10: key stability/divergence, metadata+session fallback + empty session→None, auto→None but session fallback applies, non-Responses→None, incremental→None, envelope unwrap, novel first-full then deltas + independent convos, shrink-rebase, missing/negative, eviction-restart. Thorough. Minor: `novel` linear scan O(n) + retain O(n) per call, n≤512 — fine for per-request path.
- No bugs. Verify caller uses conversation_ledger (funnel) later in proxy.

## Entry 022 — cost_tracker.rs 1-209 (chunk 1/12, in progress)
Partial OK:
- cache_economics: openai 0.5/1.0 Automatic-no-TTL, gemini 0.1/1.0 Explicit-cachedContent, bedrock 0.1/1.25 Same-as-Anthropic, default anthropic 0.1/1.25 Explicit-breakpoints-5m. Matches Python _CACHE_ECONOMICS.get fallback. Plausible.
- bucket_by_cache_mix: tokens<=0→zeros; negatives clamped; billed==0→all list; else proportional read/write, remainder list (float remainder avoids rounding loss). Doc rationale (live-zone rewrites pass read=0, prefix passes full mix, no-breakdown→list) sound. Correct.
- header_safe_transforms: smart_crush:<n>:<names> + read_lifecycle:<state>:<path> collapse to 2-part legacy shape; others passthrough. Prevents comma-joined header breakage. Correct (split(':') >=2 → first two parts).
- round_n: non-finite passthrough, else 10^ndigits round_ties_even (matches Python round half-even). Correct.
- merge_cost_stats: None→None (mirrors Python), cache net from totals.net_savings_usd or 0, compression from savings_usd or 0, clones object (or empty), sets savings_usd + compression_savings_usd (rounded 4), cache_savings_usd (rounded 4), cli_tokens_avoided + alias cli_filtering_*, both included_in_compression true. Monotonic compression-only savings_usd preserved. Correct.
- TokenRecord defaults neutral, tool_schema_saved additive but excluded from tier buckets (never billed) yet folded into totals/list savings — documented split, will verify bucketing later. cache_inferred (OpenAI inferred write priced at list not write rate) — verify use later.
- PerModel maps started (saved/sent/requests/read/write/5m/1h...). Continue chunk 210+.
- No bugs in this chunk.

## Entry 023 — cost_tracker.rs 210-359 (chunk 2/12, in progress)
Partial OK:
- PerModel continued: output keyed (model,long) for >200k tier pricing, saved_write/saved_list f64 per-request splits keyed same. Inner VecDeque costs + last_prune + PerModel. CostTracker budget_limit + period string + Mutex Inner (Arc on AppState per docs). new() inits empty + now.
- estimate_cost → estimate_cost_split with 1h=0. estimate_cost_split: lookup? else None (mirrors litellm None), clamp negatives, cw_1h clamp [0,cw_total] (handles -1 sentinel →0, over-claim →cw_total), long = is_long_context(input+read+cw_total) (whole-request tier incl. output per comment), per-tier read/write/1h rates with fallback to input_rate, total = inp*in + out*out + cr*cr + cw*cw + 1h*1h, Some if >0 else None. Correct: -1 sentinel handled, tier covers output, fallback chain matches Python .get(...,uncached) pattern. Verify pricing::lookup/is_long_context later.
- list_price_per_token (input_cost_per_token), cache_prices (None if no entry or zero base price — long-tier admission guard so flat-zero rejects even if tier non-zero; uncached=input_rate(long), read/write fallback to uncached). Sound.
- record_tokens start: input_tokens = uncached else tokens_sent when all three breakdown zero (no-usage fallback). cost via split. Post-guard invariant comment (never forward larger than original, savings>=0 by construction) — verify handlers revert later. Continue 360+.
- No bugs in this chunk.

## Entry 024 — cost_tracker.rs 360-509 (chunk 3/12)
Partial OK:
- record_tokens tail: negative saved clamped 0 + debug (wire-not-inflated, handlers revert — verify later). Lock unwrap (not poison-tolerant unlike conversation_savings novel() which recovers — INCONSISTENCY-003 minor: poison unwrap vs recover. No panic-while-holding paths known, note only).
- Headline = saturating_add(tool_schema), per-model saved/tool/sent/reqs/read/write/5m/1h/uncached. write_eff 0 if inferred else max0. billed_prompt read+write_eff+uncached, long=is_long(max(billed,tokens_sent)). output keyed (model,long). Saved buckets bucket(saved,0,write_eff,uncached) read=0 live-zone rationale (frozen prefix preserved, warm 0.1x irreconcilable — matches bucket docs). tool_schema excluded from buckets, in headline. Costs push cap 100k + prune.
- prune_old 5-min throttle, 744h cutoff (31d >= monthly). Correct.
- get_period_cost hourly/daily(month-start midnight)/daily-midnight with with_* fallback now. Filter sum. Correct.
- check_budget None->(true,inf), else remaining>0 strict. Correct.
- stats start sorted models, total, reduction saved/(saved+sent)*100 round1, Python shape. Continue 510+.
- No logic bugs in chunk. Open findings unchanged + INCONSISTENCY-003.

## Entry 025 — cost_tracker.rs 510-659 (chunk 4/12)
Partial OK:
- stats tail: per_model message=saved-tool split (tool never billed), sent, 5m/1h, reduction. cost_with_headroom per model via cache_prices(false) — NOTE base tier false always here, not long_context per-request tier. Uses cr/cw/uncached mix if any breakdown else sent*uncached. Verify: long-context turns priced at base here (understates spend >200k) while estimate_cost_split + output_cost use long tier. Possible INCONSISTENCY-004 (stats input cost base-tier vs per-turn long-tier). May be intentional (dashboard base rate) but diverges from billed. Flag for cross-check with Python cost.py. Continue to confirm.
- savings_usd list-price monotonic (saved*list, skip <=0, missing price skip). Budget reads this (monotonic basis per comment). output_cost per (model,long) output_rate(long). cache_aware = write_part*cw_price + list_part*uncached via cache_prices(long) (live-zone counterfactual, session summary prefers this). sum5m/1h totals. JSON shape cost_with_headroom (input-only, budgets/tracker), output_cost, total_cost, savings + cache_aware, budget fields. round4. Sound structure; tier question above only.
- reset_runtime clears costs + PerModel, resets prune clock. Test/debug helper. Correct (lock unwrap same poison note).
- Dashboard helpers start: find_model_input_price via find_provider_model prefix heuristics (anthropic/bedrock→claude, openai→gpt/o1/o3/o4, gemini→gemini, else none). Mirrors Python per comment. Note openai list misses o4-mini? Actually contains o4 covers o4-mini. OK. ProviderCacheInput struct started.
- INCONSISTENCY-004 (pending): stats cost_with_headroom uses base tier (false) for all models vs long-tier elsewhere. Verify Python + pricing tiers later.
- No panics (unwrap_or defaults, contains checks). Continue 660+.

## Entry 026 — cost_tracker.rs 660-809 (chunk 5/12)
Partial OK:
- Structs: ProviderCacheInput (read/write/5m/1h + requests, uncached, hit/bust counts), ProviderMissAttribution (ttl/prefix/unknown), PrefixFreezeInput (avoided/preserved/foregone), CompressionVsCacheInput (saved/lost/count), PrefixCacheStatsInput (providers + model_prices + miss + freeze + vs + tokens_saved — decoupled from PrometheusMetrics per docs, core has no proxy dep — upholds layering).
- Outputs: CacheHitRates (token/request), CacheTtlBuckets (5m/1h tokens/reqs/total/mix%/active), ProviderCacheStats (full row incl. read_discount/write_premium strings, savings/write_premium/net, label, cache_pricing_source vendored vs provider_default, ttl buckets), Totals, MissAttribution (totals + by_provider + pcts), PrefixFreeze (net_benefit), VsCache. All Serialize/Deserialize for dashboard JSON. Shape plausible vs Python builders. No logic yet (plain data). Continue 810+ (builder fns).
- No bugs in structs. Layering intact (no proxy import in this file so far — only chrono/serde/json + crate::pricing/request_outcome).

## Entry 027 — cost_tracker.rs 810-1109 (chunk 6/12)
Partial OK:
- build_prefix_cache_stats: skips 0-request providers, econ table + vendored per-model cache rates override (uncached>0 guard, read/write = cache_cost/uncached, source vendored else provider_default). savings=read*price*(1-read_mult), premium=write*price*(write_mult-1) only if >1 (openai/gemini write 1.0 → none, correct). hit rates token=read/total_input*100 round1, request=hit/req*100. TTL mix 5m/1h over observed write tokens, active buckets list. Row strings read_discount/write_premium % or none, rounded savings/net. Totals accumulate raw + savings/premium, recompute hit rates + net + TTL mix same formula. Miss attribution skips total==0, per-provider map + totals, pcts over attributed (ttl+prefix, unknown excluded from denominator — intentional, unknown not attributed). PrefixFreeze net=preserved-foregone. VsCache passthrough. All pure, no I/O, no unwrap panics (all checked, divisions guarded >0). Correct.
- Note: totals.savings_usd accumulates unrounded then rounds at end (vs per-row rounded) — rounding drift <0.5c per provider, acceptable, matches typical dashboard practice. No bug.
- Continue 1110+ (session summary + tests).

## Entry 028 — cost_tracker.rs 1110-1409 (chunk 7/12)
Partial OK:
- Session structs: CompressedRequestLog, CostSummary (input/list/output/total/cache_aware split — budgets list, headline cache_aware), McpEvents, CodexWsStats, SessionSummaryInput, CompressionSummary (incl. cli/rtk aliases: rtk_* duplicates cli_* — intentional compat, same values), CostBreakdown (cache + compression cache_aware + list), CostSummaryOutput (without/with/input/output/total/pct + provider discount beside not inside), SessionSummary (mode/api/primary/compression/uncompressed/cost + optional mcp/codex/tip skip-if-none). Shapes match Python dashboard per comments.
- summarize_uncompressed_reasons: seeds 4 zeros, passthrough→passthrough, saved>0 skip, else original>0: empty transforms→prefix_frozen, all excluded/protected→no_compressible, <500→too_small, else prefix_frozen. Mirrors Python categorization. Edge: original==0 with saved==0 falls through (no bucket) — correct (empty request, nothing to explain). Correct.
- build_session_summary: compressed filter saved>0, avg size-weighted (orig/saved sums, not mean of % — comment explains tiny-request dominance trap, correct), best by savings_percent partial_cmp fallback Equal (NaN-safe), detail "orig → opt tokens", cost fallbacks (total else input+output), compression prefers cache_aware if >0 else list (headline cache-aware, budgets list — matches CostSummary docs), provider discount beside total (never inside — reconciliability note, dwarfs compression on long sessions), without = with + compression, pct = saved/without*100. primary max-by-count else unknown, api excludes count_tokens. uncompressed filtered >0. Compression totals with cli/rtk aliases. Rounding 2/1. Sound. Continue 1410+ (tip/mcp tail + tests).
- No bugs in chunk.

## Entry 029 — cost_tracker.rs 1410-2227 (completes file, 2227 lines total)
OK:
- Tail builder: mcp/codex cloned, tip None unless mode==cache && prefix_frozen>10 → HEADROOM_MODE=token ~25-35% tip. Correct threshold.
- Tests 25+: econ defaults, header collapse, merge layers/None, estimate known/unknown/zero→None, record+stats monotonic + reduction 66.7 + savings 0.003, long-tier pricing (300k→long rates 6/22.5, threshold strictly >200k so 200k stays base, untiered opus unaffected, inferred writes don't tip tier — validates write_eff=0 tier logic from Entry 024), negative clamp + accumulate-from-0, budget unlimited/limited, empty stats, prefix stats basic/net-negative #1800 regression (net can go negative when write-dominated — honest), empty, miss attribution pcts over attributed only, TTL buckets mix, provider match, find price, session basic/weighted-avg upstream 73a6edbe (98%+10% mean 54% vs weighted 10.9% — validates weighting), bucket splits/fallback/live-mix, output+aware (600@write+400@list=0.00345 math verified in test), inferred→list, vendored-vs-table (gemini 0.25 vs 0.1), fallback provider_default zero savings, headline-excludes-discount (1.05 vs 23.62, without 12.52), tip on/off, uncompressed categories. Math in tests matches code (spot-checked 10000*3e-6*0.9=0.027 etc.). Thorough.
- Whole file: no tokio (upholds core invariant), no proxy dep (only pricing/request_outcome), all locks bounded, all divisions guarded, rounding consistent (stats 4dp, session 2dp/1dp). INCONSISTENCY-004 remains: stats cost_with_headroom base-tier vs long-tier in estimate/output/aware — tests EXPECT long-tier for output/aware but cost_with_headroom test at 35k (below tier) doesn't probe it; long test asserts output/aware long but not cost_with_headroom. So cost_with_headroom long-tier behavior untested — flag stands, verify Python cost.py later. Not a bug proof, just untested divergence.
- No logic bugs found. File complete.

## Entry 030 — ctx/mod.rs (33 lines) + ctx/store.rs 1-60 (chunk 1) + goal re-anchor
- mod.rs OK: CTX-1 storage foundation, TS port (store.ts + unified.ts), FTS5 BM25 dual porter/trigram RRF, proximity, Levenshtein, markdown chunker. Schema byte-compatible with TS DB at ~/.claude-personal/context-mode/content/<hash>.db, Headroom default <workspace>/ctx (not writing into other's dir — correct boundary, matches flags --ctx-store-dir $HOME/.claude-personal/context-mode? Wait flags line 115 says --ctx-store-dir $HOME/.claude-personal/context-mode — that IS the TS dir. mod docs say Headroom defaults to <workspace>/ctx, schema compat not reason to write into another tool's dir. POTENTIAL DRIFT: flags points at TS dir while mod docs say don't. Flag as DRIFT-003 (pending): verify default_base_dir + flag interaction later. Later phases CTX-2+ listed. Re-exports MemoryRecordStore, SessionsStore etc., snapshot, store types + RRF_K. No bugs (re-export only).
- store.rs 1-60 partial OK: byte-compatible port claim, schema/query/chunker 1:1, concurrency Mutex (mirrors ccr sqlite, sync wrapper, async is proxy job — upholds core sync contract), no silent fallbacks (open propagates, search/index Result). Consts MAX_CHUNK 4096, blank sections 3/200/5000, title 80 chars, whitespace 0.5, RRF_K 60 public with scale docs (best 2/(K+1)), STOPWORDS verbatim store.ts:51. Plausible. Continue 61-1930 chunked.
- get_goal 2026-09-11: active, tokens 8.9M, 515s. Objective intact. Continuing perpetually per goal. Staying concise to extend runway.
- DRIFT-003 (pending): flags ctx-store-dir vs mod default_base_dir boundary note. Verify later.
- No bugs in chunk.

## Entry 031 — store.rs 61-210 (chunk 2)
OK: STOPWORDS tail + is_stopword (caller lowercases, contains — O(n) over ~80 words per term, fine for query terms not docs). SortMode Relevance default / Timeline, ContentType code/prose + as_str, SearchOpts limit/source LIKE-literal/content_type/sort (TS defaults noted), IndexOpts file/hash/session/event + plain_text_lines (markdown vs plain chunker), IndexSummary, SourceMeta (CTX-5 freshness), SearchHit (title/content/source/rank negative RRF/match_layer rrf vs rrf-fuzzy), SearchTimings 5 buckets disjoint (porter/trigram incl. fuzzy FTS, fuzzy=vocab only, reader=pool wait) + add() + elapsed_ms. Chunk struct title/content/has_code. All match TS port claims so far. No bugs.

Next: store.rs 211-1930.

## Entry 032 — store.rs 211-360 (chunk 3)
OK: CtxStore conn Mutex + path + readers pool (20 concurrent 21.6s→parallel via WAL, readers don't queue) + read_only flag (RO WAL empty-schema trap → writable readers running SELECTs only). MAX_IDLE 8, MMAP 256MiB (1.89s→0.67s, VA not RSS). is_in_memory (:memory:/empty/mode=memory). tune_connection mmap via query_row (pragma returns row). conn() poison-recover (single-statement, no txn — sound, contrasts cost_tracker unwrap; good pattern). take_reader None for :memory: (second conn = empty DB) else pool pop or open (RO flags if read_only) + tune; open fail → None fallback to shared. give_reader cap 8. open() sqlite_tuning::apply + WAL+NORMAL (readers don't block, power-loss ≤1 miss, same as ccr) + init_schema. open_read_only (cold sweep: no create, no write lock, no migration — migration on owner's next write-open). Sound. No bugs.

Next: store.rs 361-1930.

## Entry 033 — store.rs 361-510 (chunk 4)
OK: path(), purge_all (DELETE chunks porter count returned + trigram/sources/vocab, CTX-6 /ctx/purge — counts porter only, trigram mirror assumed equal; if diverged, count understates. Minor note, not bug — deletes all 4). source_meta (chunk_count i64→usize narrow + indexed_at, None if missing, CTX-5 freshness). content_by_hash (CCR week-TTL vs index indefinite, reconstruction not original — multi-chunk join "\n" drifts, single-chunk exact per sampling; ORDER BY id DESC latest, chunks ORDER BY rowid insertion order, None if no source/empty; honest docs). init_schema byte-identical store.ts:463 (sources, chunks porter, chunks_trigram, vocabulary, idx label + idx content_hash migration IF NOT EXISTS, interchange preserved). No bugs.

Next: store.rs 511-1930.

## Entry 034 — store.rs 511-660 (chunk 5)
OK: index_content chunks via plain(markdown) + code count + session/event default "" (store.ts sentinel), now via SQLite strftime ISO sec-precision (TS millis — ordering same, format differs sec vs ms; byte-compat claim is schema not timestamp format — acceptable, noted). Tx atomic dedup DELETE chunks/trigram/sources by label then INSERT (empty chunks still creates source row 0/0). Dual FTS inserts porter+trigram same rows (source_category NULL always — TS parity? presumably TS also NULL; verify TS later if needed). vocab INSERT OR IGNORE from raw text. Commit. Search entry search→search_timed (hits only), search_timed takes pooled reader else shared fallback (concurrent reads). Continue 661+ pipeline. No bugs in chunk.

Next: store.rs 661-1930.

## Entry 035 — store.rs 661-960 (chunk 6)
OK: search_timed merges queries by source::title best-rank wins, timestamp T/Z normalize (SQLite space vs ISO T, unified.ts:164), Timeline sort else relevance order, truncate limit, return reader. search_with_fallback RRF→rerank top min(len,RERANK_DEPTH) + layer rrf else fuzzy (lowercase ≥3 non-stop, correct vs vocab, re-RRF + layer rrf-fuzzy; refreshStale + sessionAllowSet omitted CTX-1 documented, CTX-1b wire pending). rrf_search fetch limit*2 max10, porter+trigram FTS, RRF 1/(60+i+1) sum, stable sort desc, rank=-score. fts_search porter sanitize vs trigram sanitize, empty MATCH→[], SQL bound params only (LIKE ESCAPE + content_type + ORDER rank LIMIT), bm25 5.0/1.0 + highlight markers. source_filter escapes \ % _ → %...%. fuzzy_correct <3→None, len±max_dist candidates. No SQL injection (bound), no panics (partial_cmp fallback, ok()? fallbacks). Continue 961+.

Next: store.rs 961-1930.

## Entry 036 — store.rs 961-1260 (chunk 7)
OK: fuzzy best within max_dist 1/2/3 by len (≤4/≤12/else), exact→None (in-vocab no correct). levenshtein chars DP O(n*m), empty→len. rerank title 0.6 code /0.3 prose * hits/terms + proximity 1/(1+span/len) + phrase 0.5*min(adj/4,1) for ≥2 terms all-present; sort boost desc then rank asc (store.ts:1334). Terms ≥2 chars, stop-filtered else all, deduped, truncate 150 (20k paste 5.4s→0.48s, repeats dilute title_boost). find_all char-step (not byte+1 — panic-poison fix documented, holds store mutex). min_span sliding ptrs, adjacent gap 30. dedupe case-insensitive first-casing. MAX_TERMS 150 (65→0.47s, 1500→5.69s, lock held whole run), RERANK_DEPTH 300 (2000 partition→top10 path). sanitize porter replaces '"(){}[]*:^~ with space, drops FTS ops, dedup, stop-filter else all, truncate, join quoted OR/space; empty→'""'. UTF-16 vs UTF-8 noted CTX-1b pending non-ASCII. No bugs.

Next: store.rs 1261-1930.

## Entry 037 — store.rs 1261-1560 (chunk 8)
OK: trigram cleaner removes (no space) + trim, <3 chars→"", ≥3 filter, dedup/stop/truncate/join. join_quoted OR/space. vocab split non-alnum/_/- , lower, ≥3 non-stop unique order-preserved. markdown chunker headings (1-4), hr flush, fence intact (exact fence match close), stack pop ≥lvl, content+heading, flush helper trims empty, title from stack, has_code fence, ≤cap single else para-split \n\n with (n) suffix + ``` detect. byte_capped_prefix char-safe + single-wide progress. plain oversized line-split whitespace 0.5 break + (p.l) titles. No char-boundary panics (chars/prefix/slice at break_point from rfind ASCII space/newline — safe). Continue 1561+.

Next: store.rs 1561-1930.

## Entry 038 — store.rs 1561-1760 (chunk 9)
OK: plain chunker blank 3-200 all<5000 else fixed lines_per_chunk step-2 overlap (saturating, max1, empty break, Lines a-b fallback, oversized→split fn). blank split collapses runs (JS greedy \s), pending_newline join. take_chars scalar, build_title stack join > else current else Untitled, hr ^[-_*]{3,}\s*$, heading #{1,4}+space. line[hashes..] byte slice safe (hashes ASCII #). No bugs.

Next: store.rs 1761-1930.

## Entry 039 — store.rs 1761-1930 (completes file, 1930 lines total)
OK: fence len/starts, para split \n\n+ byte loop with utf8 len (ASCII \n safe, multibyte via len fn — continuations never mis-split since only \n triggers), utf8 len table (4-byte fallback incl. invalid — acceptable, no panic since slicing at char bounds derived from leading byte; invalid bytes could panic on text[i..i+len] if malformed — input is &str valid UTF-8 so leading byte always correct length. Safe). normalize \→/, all-slash→/, trim trailing (drive root no-op Linux). hash canonical SHA256 16 hex, Linux no fold (cfg mac/win fold — matches TS gate). content_db_path <base>/content/<hash>.db, no legacy rename (Linux equal, CTX-1b mac/win pending — documented). default_base_dir <workspace>/ctx (~/.headroom/ctx or override), None if no HOME nor override (never guess). Docs explain move from ~/.claude-personal/context-mode (mixed state, invisible) + no-migration fresh store safe-but-not-free + --ctx-store-dir old path to keep — RESOLVES DRIFT-003: flags --ctx-store-dir $HOME/.claude-personal/context-mode is explicit operator choice to keep old store, default is workspace/ctx. Not drift, documented migration path. Test hooks chunk/plain. mod tests separate file. Whole store.rs no unwrap panics on hot path (ok()? fallbacks, guards), bound SQL, poison-recover, pool caps. No bugs. DRIFT-003 resolved as intentional.

Next: ctx/store tests, sessions, snapshot, memory_records, then memory/, etc.

## Entry 040 — ctx/store/tests.rs exists + sessions.rs 1-60 (chunk 1/5)
- store/tests.rs present (verify content later — port parity tests expected).
- sessions.rs partial OK: Sessions DB port session/db.ts + new conv_prefix_chain (CTX-2 identity classifier, no TS equiv — new table documented). session_events/meta/resume/tool_calls byte-compat at <base>/sessions/<hash>.db. Mutex + WAL+NORMAL sync wrapper, proxy background thread writes (never request path). No silent fallbacks (open propagates). BUSY_TIMEOUT 120s (73s 9.7GB dedup build measured, inserts queue not SQLITE_BUSY; reads via WAL snapshot). DEDUP_INLINE 200k (<1s). dedup_index_present via sqlite_master. Sound. Continue 61-735.

Next: sessions 61-735, snapshot, memory_records.

## Entry 041 — sessions.rs 61-260 (chunk 2/5)
OK: row_count_at_most LIMIT+1 bounded 14ms on 9.7GB. create_dedup_index non-unique (MMs dups would fail unique + take store down). StoredEvent mirrors TS 557 cols, NewEvent write subset + defaults (unknown/2/zeros/empty, hash auto sha256[..16] on empty). EventInsert id/dup, PrefixTurn n/hash. SessionsStore conn Mutex + path + has_dedup Atomic (insert skips lookup until index lands, flips mid-life). conn() poison-recover (single-stmt sound, same as CtxStore). open() busy 120s + WAL/NORMAL + init_schema + dedup present check + ensure (small inline µs dedup-from-first-insert else bg thread 73s case, unindexed probe skipped — high-water keeps dups meanwhile). Warn ctx_dedup_index_failed on err. Sound. Continue 261+.

Next: sessions 261-735.

## Entry 042 — sessions.rs 261-460 (chunk 3/5)
OK: bg build own conn + busy timeout + flag flip + info/warn once-per-open (not per insert). dedup_active getter, test drop helper. path(). init_schema verbatim TS (events/meta/resume/tool_calls + idxs) + conv_prefix_chain CTX-2 (conv,turn→hash) + conv_by_session_key CTX-4 (session_key→conv seq monotonic clock-independent) + conv_injection CTX-4 (I4 replay verbatim). insert_event auto hash sha256[..16] if empty, idempotent (session,type,hash) backstop (capture high-water is fix, resume/compaction legit re-read). Probe only if index (unindexed 23.4ms vs 0.003ms on 27k group measured). Sound. Continue 461+.

Next: sessions 461-735.

## Entry 043 — sessions.rs 461-660 (chunk 4/5)
OK: dup→id/true else INSERT→rowid/false. search_events LIKE data/category scoped project+'' bucket, cat filter, id ASC LIMIT (ESCAPE \ but query not escaped for %/_ — LIKE metachars from user match broadly; TS parity? If TS also unescaped, parity holds but wildcard injection widens results not narrows — safe direction, note as MINOR-001 to verify TS). get_events by session ASC LIMIT. prefix record upsert (seen_at now), last/first/at (first-sight vs missing-row distinction documented). resume record upsert seq MAX+1 monotonic + last_seen, recent excl current seq DESC LIMIT. All bound, optional() None-safe. Continue 661+.

Next: sessions 661-735.

## Entry 044 — sessions.rs 661-735 (completes file, 735 lines total)
OK: put_injection INSERT...DO NOTHING (decided-once I4, no oscillate), get_injection optional. row_to_event 14 cols order matches schema. data_hash sha256[..16] 8 bytes hex (matches NewEvent auto + TS?). session_db_path <base>/sessions/<hash>.db same canonical hash (no worktree suffix, CTX-1b pending — documented). mod tests separate. Whole file: bound params, poison-recover, bg index, WAL, no silent fallback. MINOR-001 (LIKE %/_ unescaped in search_events) pending TS parity check. No bugs.

Next: ctx snapshot, memory_records, sessions/tests, store/tests spot-check, then memory/, etc.

## Entry 045 — ctx/snapshot.rs (287 lines) + memory_records.rs (314 lines, both complete)
- snapshot OK: CTX-4 pure deterministic, drops TS generated_at (cache-bust, I1/I4 replay byte-for-byte), order caller slice (store id ASC = TS created_at ASC). Caps 10/section, 4 queries, 400 chars, 3 recent. INJECT_SENTINEL idempotency. resume: how_to + 6 cats (intent→goal,file,error,decision,rule,git) + recent user (intent/user-prompt last 3) wrapped events/compact_count. recall: hits take10 one_line200 `- [src] content`, empty→(no prior...), TOC + static directive (retrieve on demand, don't assume). render_section dedup order cap + queries one_line80 dedup cap4, None if empty. recent last-N chrono. TOC quoted "→' + headroom ctx search. one_line whitespace-collapse + … cap codepoint-safe. Tests 5: wrapper/no-volatile, deterministic, omit-empty, recall/directive/no-vol, no-hits. No timestamps, no I/O, no panics (takes/filters). No bugs.
- memory_records OK: sidecar (index labels by id, record here keyed same; dumb JSON + id/user_id; proxy policy, core conns — boundary respected). Entities reverse index (which memories mention X) case-insensitive lower, record keeps raw. WAL+NORMAL+5s busy (shared sessions/accounts concurrent; missed pragmas fixed). Schema memories(id,user,record,now)+idx user + entities(mem,entity PK)+idx entity. conn poison-recover. put upsert (record+user), get optional, delete tx edges-first (no dangling entity→missing id) →bool, set_entities delete+INSERT OR IGNORE normalized non-empty tx (short lists rewrite, no mid-rewrite read), all_records (backfill only, stays ignorant), backfill via user_version not row-count (empty vs predates indistinguishable; no rescan), mark/reset, memories_for_entities normalized empty→[] (not everything), placeholders generated values bound (no injection), DISTINCT unordered (caller ranks), ids_with_content json_extract $.content (no migration, small scan; 3-copies top5 lesson), ids_for_user (bulk delete index clear), delete_user count (note: leaves entity edges dangling? delete_user deletes memories only, not memory_entities — DANGLING EDGES BUG? delete() removes edges, delete_user() does not. memories_for_entities could return ids with no record. Flag FINDING-009 (likely bug): delete_user should delete edges too. Verify caller clears index/edges elsewhere; if not, leak. Record). Tests 4: roundtrip, replace, delete bool, scoping. No edge-orphan test. FINDING-009 pending caller check.
- Open: FINDING-009 (delete_user edges), MINOR-001 (LIKE escape), INCONSISTENCY-003/004, DRIFT-001/002, RISK-001, etc.

Next: memory/ subsystem, ctx sessions/store tests spot-check, then core memory/, onnx_cpu, etc.

## Entry 046 — FINDING-009 confirmation (delete_user edges)
- Grep delete_user: only caller proxy ctx_backend.rs:663 clear_user_sync (ids_for_user → FTS clear per id → records.delete_user). No edge delete. memories_for_entities callers: ctx_backend.rs:379 related expansion (already-set skip + load None→skip with comment "Edge outliving its record... skip it, never fail"), plus tests 928/961/972.
- Verdict: FINDING-009 CONFIRMED as storage leak, NOT correctness bug. Dangling (memory_id,entity) rows after clear_user survive; related expansion tolerates via load None→skip (lines 383-386). Effect: edge table grows with cleared users, extra DISTINCT ids scanned then skipped per related search. No wrong results. No index corruption (FTS cleared per id). Fix would be DELETE FROM memory_entities WHERE memory_id IN (SELECT id FROM memories WHERE user_id=?) before/in delete_user tx, but NO CODE CHANGE per goal — record only.
- Severity: low (leak + minor related-search overhead). No test covers it (memory_records tests lack edge-orphan case).
- Continuing perpetual audit. Next: core memory/ (backend/models/router), proxy memory/ (14 files), etc.

## Entry 047 — core memory/mod (10 lines) + models.rs (220) + backend.rs (355) + router.rs 1-80 (chunk 1)
- mod OK: ports model/backend/routing, orchestrator stays Python (sqlite-vec/ONNX/mem0 deps). Correct boundary.
- models OK: ScopeLevel user/session/agent/turn snake_case, Memory full (id uuid, content, user/session/agent/turn, created/valid_from/until, importance 0.5 default, supersedes/by/promoted/chain, access/last, entity_refs, metadata). Default uuid + now, scope narrowest-wins (turn>agent>session>user), is_current valid_until None, new builder. Tests 10: default, 4 scopes, superseded, builder, serde roundtrip + scope serde agent, lineage. Sound. No bugs.
- backend OK: sync trait (async in proxy where tokio lives — upholds core tokio-free), SearchResult memory/similarity/rank, trait save/search/update/delete/get/supports_graph/vector/close mirrors Python Protocol. Relationship, UpdateError Display. MockBackend Mutex Vec: save (importance/session, ignores entities/rels/meta — test mock, OK), search filter user+contains + fake similarity 1-i*0.1 + top_k (sorted desc — already desc, stable), update clone-new + supersede chain + push (original marked, get returns original not updated? get by id finds first match — updated has NEW id (default uuid from clone? Actually updated=existing.clone() keeps same id? Code: updated=existing.clone(), content=new, supersedes=existing.id, then push updated. Both have SAME id (clone preserves id). get_memory finds first (original). So after update, get(old id) returns original (superseded) not updated — both share id. Is that intended? Python supersession mints new id? Here clone keeps id, so two rows same id. delete removes both. search returns both? Filter contains query — old content may not match new query. Ambiguous. Flag MINOR-002: MockBackend update clones id (duplicate ids) vs mint new. Test only asserts updated.content + supersedes + original superseded_by, not get(new). So test passes but model allows duplicate ids. Test-only mock, low severity. Record). delete retain, get find, supports false, close noop. Tests 11 cover save/get/user/content/top_k/supersede/notfound/delete/caps/rank/display/rel default. Sound except MINOR-002.
- router 1-80 partial: GH#462 bleed fix, Project/User/Global, LRU path cache. CWD_PREFIXES 3 formats, BASENAME_ALLOWED alnum._-, modes, RequestContext headers/system/base/override, ResolvedScope mode/path/display/key, RouterConfig mode/root/global/max_open/fallback empty vs global. Continue 81-818.
- MINOR-002 (test mock duplicate ids on update). No prod bugs in chunk.

Next: router 81-818, then proxy memory/.

## Entry 048 — core memory/router.rs 81-818 (completes file, 818 lines total)
OK: default /tmp root, 16 LRU, empty fallback. Resolver tiers explicit-id (sanitized, empty→next) > cwd header > override > system prompt (first prefix hit, \n-terminated). BackendRouter global/user (sanitized else default, users/<u>/memory.db)/project (projects/<key>/memory.db else global vs empty-fail-closed with unused global path + None key — caller checks key). acquire LRU touch-to-back + evict front over cap, open snapshot/count (unwrap on Mutex — poison panic vs recover pattern elsewhere; INCONSISTENCY-003 addendum: router also unwraps. Same low risk). Helpers: header case-insensitive first-nonempty, cwd extract first-prefix \n slice trim, identity trim + trailing /\, basename or root, sanitized empty→project, key base-digest16 (SHA256 normalised, 8 bytes hex — matches ctx hash style), sanitize collapse non-allowed to single - + trim -._ + 64 chars + empty→"" (callers fallback), hex fmt, system extract Anthropic str/list text join + OpenAI system msg str/list else "". Tests 30+: resolver tiers/priority/none, global/user/default/empty/global-fallback/resolved, LRU cache/evict/touch, sanitize 5, cwd extract 5, system 4, identity 4. Note: sanitize `ch as u8` truncates non-ASCII to low byte — non-ASCII allowed check is lossy (e.g. é→0xA9 not in allowed→'-'). Safe (sanitizes) but collides distinct Unicode names to same dashes. Acceptable for FS key (hash disambiguates: key includes digest of normalised full path, so collision only in display part). No bug. Whole router sound, GH#462 isolation via path per project. No prod bugs.

Next: proxy memory/ 14 files, core onnx_cpu/output_savings/parser/paths/..., etc.

## Entry 049 — onnx_cpu.rs (29) + paths.rs (290) + parser.rs 1-60 (chunk 1/9)
- onnx_cpu OK: AVX2 gate for precompiled ORT (Magika + embeddings), x86→is_x86_feature_detected avx2 else true. Matches lib.rs 1.24 + #1278 SIGILL + #1723 notes. pub(crate). No bugs. Note docs mention ort-download-binaries* but Cargo uses load-dynamic — stale doc phrase? Cargo fastembed/ort use load-dynamic (dlopen) not download-binaries static. Doc says "shipped by ort-sys (via fastembed ort-download-binaries*)" — DRIFT-004 (minor docs): feature name outdated vs current load-dynamic. Behavior (AVX2 gate) still correct. Record.
- paths OK: config ~/.headroom/config + workspace ~/.headroom, precedence explicit>env>derived>default, pure no mkdir (ensure_* do), no cache re-read env (test overrides). Partial port (subscription/savings/toin only, dashboard/memory omitted + resolve helper for future). env trim blank→None, expanduser ~/ + home $HOME then USERPROFILE win (matches Python Path.home/home.expanduser). resolve explicit non-empty + expand, else env expand, else derived. workspace $WS or $HOME/.headroom else ./​.headroom (fallback . when no HOME — never guess home, matches default_base_dir None pattern). env_is_set distinguishes chosen vs fallback. config $CONFIG else $WS/config else ~/.headroom/config. ensure mkdir_all. Resources savings/output/toin/sub/events/copilot with legacy envs. Tests 6 with env_guard serialize (process env mutation — correct). Sound. Used by default_base_dir (Entry 039) + savings ledger. No bugs.
- parser 1-60 partial: port parser.py, Block atomic (text/tokens/hash/msg idx/tool/waste), cross-msg re-read (same bytes >1 pos) + re-issued (same args, bytes differ) both skip first + polling gap ≤3 advance baseline no count (consecutive tool turns 2 apart + thinking nudge 3). Waste detect in waste_signals. REREAD_MIN 50 (ok/empty not evidence), GAP 3, overhead 4/10, RAG markers 6. Continue 61-1284.
- DRIFT-004 (docs): onnx_cpu feature name download-binaries vs load-dynamic.
- No prod bugs.

Next: parser 61-1284, waste_signals, etc.

## Entry 050 — parser.rs 61-460 (chunk 2-3/9)
OK: RAG regex (?i) 6 markers OnceLock expect-valid. CCR marker Retrieve hash=|<<ccr:>>. compute_hash MD5[:16] parity (grouping only, never persisted — parity for fixtures). is_rag. BlockKind 7 + as_str Python Literal. BlockFlags struct (4 keys only vs Python dict; tool_call_id Option mirrors present-None=absent). Block kind/text/tokens/hash/source/flags. json_dumps parity: separators ,/: vs ", "/": ", ensure_ascii (non-ASCII → \u, astral surrogate pair), control <0x20 \u, \" \\ \n\r\t\b\f. write_json sort_keys/compact, object map[key] (BTree? serde_json Map preserve_order per Cargo — sort only if flag, else insertion order — matches Python sort_keys=False insertion order. Correct). python_str bare str else repr; repr None/True/False, number int vs 1.0 float (.1 when fract 0 but not int — matches Python 1.0 vs 1), str single-quote prefer (double only if ' and no "), escapes \\ \n\r\t + quote, containers [..]/ {k: v} ", " join (insertion order — Python dict order, serde preserve_order matches). is_truthy (null/false/0/""/[]/{} false). get filters null. coerce non-object→{} (over-compress malformed cheaper than fail — documented). canonical_call_key str-reparse then dumps sort+compact else python_str, hash name\0canon (key-order normalized). extract_tool_result Anthropic str/list text + Strands toolResult text/json (json via dumps default seps) + images skipped, object→dumps, other→python_str. parse_message splits text vs tool_result/use parts (Anthropic tool_result/tool_use + Strands toolResult/toolUse, str parts→text). No panics (expects only on static regex valid). Continue 461+.

Next: parser 461-1284.

## Entry 051 — parser.rs 461-680 (chunk 4/9)
OK: kind role→System/User(RAG?Rag:User)/Assistant/Tool(tool)/Unknown; tool role flag id; waste>0 attach. tool_result nested toolResult/toolUseId vs tool_use_id, non-object skip, empty skip, tokens+4 hash. container skipped if empty + tr present (no overhead-only block). tool_use nested toolUse/toolUseId vs id, name truthy else unknown, args input else {}, text name(dumps sort non-compact), key canonical, tokens+10, no waste. OpenAI tool_calls truthy array, coerce, func name .get default only absent (null→None via python_str — matches Python .get), args absent→"" string, text name(python_str args) (not dumps — Python f-string str(args) parity), function_name str-only, key truthy-else-unknown, id. Empty→Unknown placeholder tokens 4 hash "". parse_messages aggregates per-block waste (6 fields, not reread yet) + Pass1 groups ToolResult ≥50 by hash. Sound. Continue 681+ (reread/reissue/compressed attribution).

Next: parser 681-1284.

## Entry 052 — parser.rs 681-900 (chunk 5/9)
OK: Pass1 groups ToolResult≥50 by hash insertion-ordered; prev=first source; same-msg skip; gap = cur-prev ≤3 polling advance-no-count else add tokens + newly_counted; total reread += ; counted_results extend. Compressed attribution only if same len: re-parse compressed[first.source] blocks join \n, marker regex + !contains(first.text) → reread_compressed += (marker replaced first serve, repeat is over-compression; lossless no-marker not attributed — documented). Pass2: results by call_id first idx (non-empty), calls grouped by call_key order; same polling skip; result via call's tool_call_id → results map; skip no-result/<50/already-counted (no double-bill); add + mark. Breakdown BTree kind→tokens. Tests start WordTokenizer (Python same rule), hash md5 vectors, string block 3+4, roles, multimodal join \n images skipped, Anthropic tr-only skips container. Sound. Note gap subtraction `cur-prev` safe (cur>prev? group order is block order = source order non-decreasing, same-msg skipped, so cur≥prev; no underflow. If out-of-order source_index ever (blocks from same msg share idx, msgs iterate asc) — monotonic. Safe). Continue 901+ tests.

Next: parser 901-1284.

## Entry 053 — parser.rs 901-1284 (completes file, 1284 lines total)
OK: tests 25 — container kept w/ text, Bedrock toolResult text+json (dumps default seps `{"b": 2, "a": 1}`), tool_use block + hash c4e011757b24743b + tokens 4+10, Bedrock toolUse, OpenAI tool_calls null content→1 block python_str args, key-order canonical, empty→Unknown, RAG reclass + negatives, breakdown sums, big_result 60w=64t, distant reread 64, polling chain 0, gap 3 incl /4 counts, small floor 0, CCR marker attribution + total excludes compressed subset (no double), lossless no-attrib, len-mismatch skip, reissue diff-bytes counts 64, identical no-double-bill, waste accumulate base64, dumps non-ASCII café→\u00e9 + astral surrogate pair, seps/sort 3 forms. All match code paths. Whole parser: parity-critical rendering verified by vectors, no unwrap panics (expects only static regex), gap arithmetic safe, double-count guards sound. No bugs.

Next: waste_signals, perf_analyzer, pricing, proxy/, relevance/, etc.

## Entry 054 — waste_signals.rs (357 complete) + pricing.rs 1-200 (chunk 1/4)
- waste OK: diagnostic not decision, 8 fields (4 here + reread x2 via parser + 2 always-zero dynamic/repetition summed by parser but never produced — honest zero). total excludes compressed subset (no double), is_empty, to_map Python keys (drops _tokens, BTree sorted — test asserts sorted order base64/dynamic/html/json/repetition/reread/reread_compressed/whitespace), non_zero filters. detect: empty→zero, html tags+comments (?s multiline — Python [\s\S] parity), base64 50+{0,2}, ws 4sp/3nl savings = count(runs)-count(joined space) saturating, json (?s)\{.{500,}\} + >500 tokens. Overlap intentional (blob in JSON counts both — attribution not partition). Tests 13: empty/prose/html/multiline/base64/floor/ws/cheap-json/bloat/total-subset/total-sum/keys/nonzero. Sound. No bugs.
- pricing 1-200 partial OK: LiteLLM vendored subset claude/gpt/gemini, exact-then-longest-prefix (mirrors _resolve), staleness ~2026-07 per-1M→per-token, refresh follow-up. LONG 200k strict > (matches cost tests). ModelPricing + accessors input/output/read/write/1h with tier fallback (info.get(above) or base; 1h→5m fallback, no tiered 1h in table). per_1m/ttl/long builders (Anthropic 1.25/2.0 from usage ephemeral_5m/1h; Sonnet 4/4.5 tiered 3→6/15→22.5/0.3→0.6/write 1.25x tier; Opus/4.6+ flat). TABLE starts Fable/Mythos 2x Opus. Continue 201-673 (table + lookup).
- No bugs in chunk.

Next: pricing 201-673, then proxy/, relevance/, etc.

## Entry 055 — pricing.rs 201-673 (completes file, 673 lines total)
OK: TABLE ordering longest-prefix wins (fable/mythos 10/50 before claude- catch 3/15 else under-report half; opus 4 retired 15/75 vs 4-5..8 5/25; sonnet-5 explicit 2/10; 4-6 flat 3/15 before 4-prefix tiered else 4.6 gets 4.5 tier; sonnet-4 tiered 3→6/15→22.5/0.3→0.6/3.75→7.5; haiku tiers; claude- fallback Sonnet-class; spark free 0/0 both spellings (alias + upstream id) else $3/M misbook free as Sonnet — fixed; gpt/o/gemini + fallbacks. lookup verbatim then gateway unwrap via tokenizer::name_candidates (bedrock/vertex/us./openrouter) then prefix; unknown→None blended. estimate split clamps, 1h carved not added, -1 sentinel→plain 5m, long=input+read+cw_total, fallback read→fallback/write→input. Tests 24: 1h 6.25 vs 10 + carve + sentinel + no-1h fallback spark, exact/prefix, spark free both names cost 0, longest beats fallback, opus family, gen5, fable, opus 4-5..8, gpt/gemini/o, fallback, unknown, estimate tier 28.5 vs 1.8, cache 0.03/0.375/0.6/7.5, tier family, flat no-tier, threshold strict, long prompt tier, fallback, clamp, wrapped resolve, unknown still none. Math verified (1M tier 6+22.5=28.5 etc.). Resolves INCONSISTENCY-004 context: tier whole-request repricing confirmed, cost_tracker long-tier use consistent. Staleness ~2026-07 best-effort documented. No bugs.

Next: core proxy/, relevance/, request_outcome, retry, rollout, savings_ledger/tracker, session_sticky, signals/, etc.

## Entry 056 — proxy/mod (3) + relevance/mod (64) + signals/mod (61)
- proxy/mod OK: re-export rate_limiter only. No logic.
- relevance/mod OK: BM25 keyword (UUID/field=value exact) + Embedding future ONNX + Hybrid fusion (BM25+boost fallback when stubbed). Trait = Python ABC. Factory create_scorer hybrid default / bm25 / embedding (stub Err mirrors Python RuntimeError when ONNX not ready) / unknown Err list. Lowercase tier. Sound. Verify bm25/embedding/hybrid impls later.
- signals/mod OK: classify vs mutate layering (transforms mutate, signals classify; line importance shared by 4 compressors → crate root). Maturity curve pattern→parser (unidiff/tree-sitter done)→ML head on bge-small (see README). Tiered composition not inheritance. Per-granularity traits. No silent fallbacks (no NoOp/zeros; confidence carries uncertainty). Re-exports keyword/line/tiered. No logic. Verify impls + README later.
- No bugs.

Next: rate_limiter, relevance impls, signals impls, request_outcome, retry, rollout, savings_*, session_sticky, sqlite_tuning, subscription, thinking_tokens, tokenizer, tool_*, transforms/, etc.

## Entry 057 — rate_limiter 1-80 + request_outcome 1-60 + retry 1-60 + session_sticky 1-60 (chunks, in progress)
- rate_limiter partial OK: token bucket per key/IP, MAX 1000 buckets (DoS spoof cap), STALE 600s, BucketState tokens/last, TokenBucket requests+tokens per min f64 + dual Mutex maps, Result allowed/wait, Stats. new u32→f64. refill elapsed*rate/60 cap rate, update now. Continue 81-294 (check/acquire/cleanup).
- request_outcome partial OK: canonical completed-request value, emit order metrics→cost→log→PERF + output_shaper hook. Observation only, provider-native; optional cache splits neutral 0/false (forget→zeros not wrong). round half-even. Fields identity incl. routed_from (reroute cost diff), status 0 treated success, >=500 failed funnel (no save inflation), attempts 0→1 normalized, provider usage Optional (never presented as billed if omitted). Continue 61-1165.
- retry partial OK: port helpers.py 919-960, 429+529 retryable. jitter min(base*2^attempt,max)*(0.5+j) with caller jitter + production xorshift nanos (no rand dep, thundering spread only). retry_after uncapped (distinguish safe-wait vs exceeds-open). Continue 61-198.
- session_sticky partial OK: collapses 3 OrderedDict LRUs (Beta/Tool/Ccr) to generic SessionSticky<T> keyed "{provider}:{session}" Mutex LruCache, cap 1000 default, 0→1, len/get-clone/insert (move_to_end via get/put). Continue 61-179.
- No bugs in partials.

Next: finish these 4 files, then remaining core.

## Entry 058 — rate_limiter 81-294 + retry 61-198 + session_sticky 61-179 (all complete)
- rate_limiter OK: cleanup stale >600s both maps + debug, check_request entry or new(full rate) + refill + >=1 consume else wait (1-avail)*60/rate, check_tokens same with count f64, stats active=request keys only (token keys not counted — minor undercount, note MINOR-003: stats.active_keys misses token-only keys; harmless dashboard). Cap check `>1000` then cleanup (not hard cap — if no stale, grows to 1001+; DoS bound is lazy not strict. Note MINOR-004: > vs >= + cleanup-only bound allows unbounded non-stale growth. Low risk, localhost proxy). Tests 10: limits/first/up-to-limit/independent/tokens/partial/stats/refill/concurrent Arc 10x10. Deadlock risk: check_request holds request lock then takes token lock inside cleanup (order request→token); check_tokens only token; stats only request. No reverse order → no deadlock. Correct.
- retry OK: date fallback RFC2822→ms max0, capped wrapper min(max), exceeds_cap is_some_and > (unparseable false→backoff — correct), next_delay ceil min(u64MAX) else backoff (over-cap passes, loops own cap — documented C5). Tests 13: statuses, bounds 50/150, growth, cap, range 100x, numeric/cap/over-cap/next ceil/0/passthrough/u64 clamp/negative/garbage/date-past. Correct. pseudo_random xorshift nanos (same-ns diverges via mul+mix). No bugs.
- session_sticky OK: get_or_insert first-wins, update RMW single lock (beta union), reset, Default 1000. Tests 8: missing/insert/evict/touch/first-wins/merge/zero→1/reset. Unwrap poison (INCONSISTENCY-003 addendum again). No bugs.
- request_outcome still open 61-1165. MINOR-003/004 low notes added.
- No prod bugs in this batch.

Next: request_outcome 61-1165, rollout, savings_*, sqlite_tuning, subscription, thinking_tokens, tokenizer, tool_*, transforms/, etc.

## Entry 059 — request_outcome.rs 61-460 (chunk 1/6)
OK: conv key/total (Responses cumulative vs frozen novel — matches conversation_savings), cache fields neutral, response_cache distinct, output thinking Option (None≠0 Anthropic no-count; inferred tokenizer vs provider scale) + stop max/length control loop + turn_index (wasted cost needs position). Timing, transforms, waste, num/turn/msgs/tags/client/project. Methods: cache_hit read>0|response_cache, visible output-thinking max0 (tokenizer scale disagree 1-2), hit_pct read/(read+write) round half-even 0 if none, savings_pct 0 if orig≤0, basis fresh=write+uncached else cache_read if read>0 && opt>fresh (favours proxy at boundary — documented), cost usd basis rate (read vs input, fallback ledger default, unknown→fallback) * saved, inflated max(opt-orig,0) diagnostic not feeding saved/attempted (no double via retrieval-drawback). StreamParams neutrals + from_stream: messages else Gemini contents→role/content (parts text truthy str(null→None, ""→None, non-str value_to_str True/False/None/else json), role model→assistant else user), system or systemInstruction else Null, log gating (!full→None/None; orig→orig/items else items/None), turn_id via turn_id::compute, routed None (claude→claude rewrite not reroute), 200/1/None, attempted=opt+saved. value_to_str True/False/None parity. summarize a*2 b insertion-ordered none if empty. Sink trait record + default noop record_tokens/log + shaper. Sound. Continue 461+.

Next: request_outcome 461-1165.

## Entry 061 — sqlite_tuning (25 complete) + turn_id.rs (283 complete) + rollout 1-80 partial
- sqlite_tuning OK: Once MEMSTATUS 0 before first conn (FTS many small allocs serialize on global mutex, 20x measured; nothing reads counters). Unsafe ffi, silent decline keeps counters (speed only). Called by CtxStore::open (Entry 032). No bugs.
- turn_id OK: stable turn hash prefix to last user TEXT (no tool_result) rolls on new prompt, strips cache_control (else per-request id), parity json.dumps sort_keys/default=str/ensure_ascii (keys sorted ", " ": ", non-ASCII \u + astral surrogate, matches parser json_dumps but separate impl — duplication noted: parser.rs json_dumps + turn_id dump + python_str/python_repr triplicate parity renderers. DRIFT-005 minor: 3 parity renderers risk divergence; tests pin each. Record). SHA256 model\0system\0prefix hex[..16]. System str/null/blocks normalized + stripped. Tests 10: strip recursive, dumps sort/space + ensure_ascii café→\u00e9 etc., empty→None, tool-only→None, string→16 hex, cache no-change, deterministic, rollover, system array normalized. Sound. Used by from_stream (Entry 059). No bugs.
- rollout 1-80 partial: channels Stable/Beta/Canary/Dev ordered allows(>=), FromStr aliases (prod/production→stable, preview→beta, nightly→canary, dev; -→_ normalized, trim lower). Features NativeBedrock/OpenAIResponsesStreaming/CanaryProbe. Continue 81-444.
- DRIFT-005 (minor): triple json parity renderers (parser json_dumps/python_repr, turn_id dump). Record.
- No prod bugs.

Next: rollout 81-444, savings_*, subscription, thinking_tokens, tokenizer, tool_*, transforms/, ccr/, etc.

## Entry 063 — thinking_tokens.rs (352 complete) + tokenizer/mod 1-80 partial + savings_tracker noted 3319 lines
- thinking OK: unknown≠0 (Anthropic no count; 0 asserts no thinking, corrupts mixed averages). ThinkingTokens tokens/inferred, known, visible max0 (tokenizer scale disagree 1-2). as_int missing vs zero (Null/Bool→None, num i64/u64/finite f64→i64 max0, str parse max0 — NOT flooring like proxy _usage_int — deliberate). details output/completion reasoning first-usable (gateways echo both). anthropic text thinking+redacted data join \n (redacted approximation flagged inferred). extract_from_usage details→reported else thoughts→reported else unknown. extract payload usageMetadata→reported; usage reported→return; else Anthropic text empty→content-array? genuine 0 else unknown; non-empty→estimator? inferred max0 else unknown (no fabricate). Tests 12: chat/responses/gemini/non-dict/missing vs reported-0/no-estimator unknown/with-est inferred/redacted/genuine-0/no-content unknown/visible clamp. Matches RequestOutcome thinking Option docs (Entry 059). No bugs.
- tokenizer/mod 1-80 partial: tiktoken byte-equal OpenAI/o, HF tokenizer.json (Cohere/Llama/Mistral/Qwen/BERT/T5), Estimating chars/cpt Anthropic/Gemini. NOT used by proxy yet Stage2 library-only; NOT Anthropic real (estimation = Python); NOT SentencePiece (Gemini SP unpublished → estimation). Overheads 4/3, blob 50k sample 20k/2k, image 1600 audio 200 (match Python). Trait Send+Sync, ""→0, det, non-empty ≥1. Continue 81-572 + impls.
- savings_tracker 3319 lines queued (SCHEMA v4, 60-min rollover, 50 projects, 5000pts/365d, atomic tmp+fsync+rename, vendored pricing blended fallback, BTree not insertion — no byte parity needed). Will chunk.
- No bugs in batch.

Next: tokenizer 81-572 + impls, savings_ledger/tracker chunked, subscription, tool_*, transforms/, ccr/.

## Entry 064 — tokenizer/mod 81-200 (chunk 2/5)
OK: Trait count_text/backend + count_messages default OpenAI-style (MESSAGE_OVERHEAD 4/msg + role + content str/array + tool_calls + function_call legacy + name+1 + REPLY 3 — matches Python BaseTokenizer). content_parts: text→count, image 1600 / audio 200, tool_result str→count else serialized, tool_use name+input serialized, empty-type Strands text/toolUse/toolResult. Bound checks via and_then/unwrap_or. No panics. Continue 201-572 (serialized/blob sampling/registry).

Next: tokenizer 201-572 + estimator/hf/mistral/registry/tiktoken.

## Entry 066 — estimator.rs (196 complete) + registry.rs 1-200 (chunk 1/3)
- estimator OK: CJK 1.5 vs Latin cpt (4.0 default, 3.5 Claude), scalar count not bytes (matches Python len), formula max(1,int(other/cpt + cjk/1.5 +0.5)) round-half-up via cast (non-neg). Ranges byte-identical Python CJK_PATTERN. new asserts >0. Tests 12: empty, 4cpt vectors, Claude 3.5, unicode chars not bytes, CJK/Kana/fullwidth, mixed, min1, det, 1M no-overflow, reject 0/neg, backend. Sound. No bugs.
- registry 1-200 partial OK: HF opt-in longest-prefix wins (no bundle/network in core) > Tiktoken OpenAI/o > Estimation (Claude 3.5, Gemini/Palm/Command 4.0 else 4.0). detect_backend lower + gpt/o/text-embed/davinci/curie/babbage/ada/code → Tiktoken else Estimation. name_candidates lower + strip / left-to-right + dotted splits (bedrock vendor.model + region variants) deduped, exact first (no correct change). Fixes wrapped-id estimator deviation (+15% EN, -33% JSON, -38% logs — measured). get_tokenizer candidates HF→Tiktoken(for_model ok)→family estimator (most-unwrapped known else default). family lower already (no realloc). HF table OnceLock RwLock prefix lower, longest wins, replace on re-register, clear for tests. Continue 201-465 (hf download/try_register + tests + tiktoken/hf/mistral files).
- No bugs in chunk.

Next: registry 201-465, hf_impl, tiktoken_impl, mistral, then savings_*, etc.

## Entry 067 — registry.rs 201-465 (completes file, 465 lines total)
OK: try_register_hf download+register independent per-model (gated Llama HF_TOKEN failure isolated). lookup_hf lower longest-prefix wins (insertion-order independent). test_support shared global lock (parallel cargo test cross-module clear race fixed) + tiny WordLevel JSON + poison-recover + clear on acquire/drop. Tests 15: openai tiktoken list, non-openai estimation, case-insensitive, density via back-compute 35→10 Claude /40→10 Gemini, HF wins over estimator, HF overrides tiktoken (deliberate override pinned), longest wins, case-insensitive reg, clear resets, unrelated estimate, detect ignores runtime, wrapped unwrap vectors (bedrock dotted + openrouter slashed), exact first, wrapped openai→tiktoken, wrapped claude density equal bare (fixes -38% logs). Poison expects (registry poisoned) — test guard recovers, prod paths expect (panic on poison). INCONSISTENCY-003 addendum: registry expects vs stores recover. Low risk (poison only after panic holding lock). No bugs. Used by pricing lookup (Entry 055) via name_candidates — consistent.

Next: hf_impl, tiktoken_impl, mistral, savings_ledger/tracker, subscription, etc.

## Entry 068 — hf_impl 1-60 + tiktoken 1-60 + mistral 1-60 (partials)
- hf partial OK: real BPE/Unigram/WordPiece via tokenizer.json; from_bytes/file/pretrained (Hub cache, blocking → main/spawn_blocking, HF_TOKEN gated); NOT bundled (MB bloat, lazy download). Arc cheap clone. Continue 61-314.
- tiktoken partial OK: same BPE tables Python → byte-equal IDs/counts. LazyLock Arc per encoding (o200k/cl100k/p50k/r50k, expect panic = programmer error). for_model → UnknownEncoding else fallback estimation via registry. Continue 61-319.
- mistral partial OK: ports MODEL_TO_VERSION + HF slice; mistral_common Python-only not installed → Python falls back to generic estimation, so Rust parity = absence of special handling (no detect_backend touch, no count_messages override, no density). Opt-in content parity via try_register_default_mistral (public tokenizer.json; template overhead still differs, generic 4/3). Table v3 tekken vs v1. Continue 61-440.
- No bugs in partials.

Next: hf 61-314, tiktoken 61-319, mistral 61-440.

## Entry 069 — hf 61-180 + tiktoken 61-180 (chunks)
- hf OK: from_bytes/file/pretrained (main rev, cache, blocking ureq; Hub vs Load errors; from_file reuse keeps load identical). count empty→0, encode(text,false) no specials (encode_ordinary spirit, provider specials differ → don't over-charge), Err→0 not panic (proxy must flow). Tests start tiny WordLevel. Continue 181-314.
- tiktoken OK: empty→0, encode_ordinary byte-equal (specials literal tolerance vs Python raise disallowed_special=all — documented, proxy tolerance). encoding_for lower, order load-bearing (4.1/4.5 before 4, gpt-5 o200k else cl100k CJK +33% overcount fix, o4 o200k), code/davinci p50k, legacy r50k, else Unknown→estimation. Tests start o200k vectors. Continue 181-319.
- No bugs in chunks.

Next: hf 181-314, tiktoken 181-319, mistral 61-440.

## Entry 070 — hf 181-314 + tiktoken 181-319 (both complete) + mistral 61-180 partial
- hf complete OK: tiny WordLevel tests (empty/known/OOV/det/unicode/file roundtrip via temp dir nanos unique/ignored Hub download gpt2 2tok/invalid ""→Hub not Load). No network in default tests. Clone Arc ptr_eq. No bugs.
- tiktoken complete OK: o200k vectors hello1/Hello4/fox9, det 1000x, unicode 5 scripts ≥1 + bound, 1MB ~250k bound, dispatch table 17 models, unknown→Err, case-insensitive, Arc shared, newer o200k (4.1/4.5/5/o4) vs plain 4 cl100k, gpt5 constructible. Order + CJK fixes pinned. No bugs.
- mistral 61-180 partial OK: PREFIX version order (open-mistral v1 before nemo v3 note), version() direct→prefix→v3 default (v2 valid unused). Repos 7B/Nemo/Small/Large/8x7B/8x22B + added Code/Ministral/Pixtral (Python had none, estimation fallback) + Pixtral community mirror (tekken vs tokenizer.json). MODEL_TO_REPO exact + PREFIX most-specific (dated names miss in Python, hit here — intentional improvement). Continue 181-440 (try_register + tests).
- No bugs in batch.

Next: mistral 181-440, then savings_*, subscription, tool_*, transforms/, ccr/.

## Entry 072 — tool_schema_savings.rs (115 complete) + subscription/mod.rs (84 complete)
- tool_schema OK: compaction in-tokens (already in saved, never add) vs deferral additive tags (never saw counting). Headline = saved + tags. New feature rule: move array→fold handler, defer→add tag name. Core shared by PERF/perf_analyzer/proxy stats. Tags 2 (tool_search_deferred, turn_hook_saved) only when Headroom deferred (not client tool-search). Parse trim i64 else 0 saturating, headline saturating max0 (inflation reverted, negative artifact). Tests 5: empty, sum, ignore unrelated, non-numeric 0, headline+clamp. Used by outcome ledger gate + PERF total (Entries 059-060). No bugs.
- subscription/mod OK: OAuth window tracking port; models serde cross-compat, base trait+registry, client token+fetcher (HTTP/async in proxy), session JSONL breakdowns, tracker pure state machine. env_guard shared (CLAUDE_CONFIG_DIR parallel-test race fixed, poison-recover). utc_now, to_utc Z secs (Python _to_utc_iso), parse Z/naive UTC (mirrors _parse), round half-even. No logic. Continue base/client/models/session/tracker.
- No bugs.

Next: subscription base/client/models/session/tracker, savings_ledger/tracker, tool_exclusion, output_savings, perf_analyzer, persistent_metrics, etc.

## Entry 073 — subscription/base (159 complete) + client.rs (139 complete) + tool_exclusion 1-80 partial
- base OK: QuotaTracker key/label/avail/stats-None-omits, Registry duplicate-reject + all (avail+Some only) + single + len/empty. Unwrap poison (INCONSISTENCY-003 addendum). Tests dup + filter. No async lifecycle (proxy owns loop), Codex/Copilot out of scope (only Anthropic in practice). Sound.
- client OK: order explicit>env CLAUDE_CODE_OAUTH_TOKEN trim>creds CLAUDE_CONFIG_DIR else $HOME/.claude/.credentials.json claudeAiOauth.accessToken non-empty + expiresAt ms -60s buffer (missing expiry→accept). No HTTP in core (Fetcher trait proxy reqwest impl, fake for tests). Tests env priority/file unexpired/expired with env_guard + tempdir. Sound. USAGE_URL + BETA oauth-2025-04-20.
- tool_exclusion 1-80 partial: defaults Read/Glob/Grep/Write/Edit/WebSearch/Fetch/view/retrieve/read_file(Cursor LOSSY worse than byte-exact fold)/Skill (instructions invert not degrade) + lowercase variants; CSV without lowercases (case-insensitive match, no --help pad); VERBATIM WebSearch/Fetch/view/retrieve (lossless fold rewrites JSON breaks; retrieve removal reopens cross-turn loop). Continue 81-359 (byte-exact + matching).
- No bugs in batch.

Next: tool_exclusion 81-359, subscription models/session/tracker, etc.

## Entry 074 — tool_exclusion.rs 81-359 (completes file, 359 lines total)
OK: byte-exact Read/read/read_file/Skill/skill (Edit anchor breaks if folded; both halves needed). aliases mcp__a__b ↔ mcp_a_b ↔ bare (both spellings + bare must match). glob fnmatch anchored ^$: *→.*, ?→., [...] !→^ (first ^ escaped literal), \→\\\\, unterminated [→literal \[ + escaped rest (fnmatch parity), other regex-escaped (. not wildcard). is_excluded empty→false, aliases exact + lower-exact (set lookups) then glob lower (regex ok else false). verbatim/byte-exact/ccr wrappers (cached Headroom 3 spellings; is_ccr via single CCR name + aliases/glob — covers mcp__Headroom__retrieve). Tests 9: case-insensitive, empty, glob mcp__*, 3-spelling aliases, literal dot, classes [!], verbatim vs Read, lossy read/skill cases, byte-exact only reads/skills. Sound. No bugs. Used by offload/compression gates (verify callers later).

Next: subscription models/session_tracking/tracker, savings_ledger/tracker, output_savings, perf_analyzer, persistent_metrics, etc.

## Entry 075 — subscription/models.rs 1-320 (chunk 1/2)
Partial OK: get_f64/i64 num+str (f64→i64 cast for i64). Window used/limit/util/resets parse, seconds max0, to_value round2 + ISO + seconds. synthesize render None→zeros/nulls false; cached + false flags; no resets→cached; past→used min(limit) + util + advance resets while ≤now + seconds + true/true. Extra cents→usd (limit 2dp, used 4dp, util 2dp). Snapshot 5h/7d + opus/sonnet optional + extra + polled now + token[..8]; nonempty guard null/empty/false (Python `if key and data[key]`); to_value omits None opus/sonnet. WindowTokens input/out/read/write5/1h/total + by_model + weighted (opus2/sonnet1/haiku0.5 — verify weights later) total_raw sum. Sound. Continue 321-624 (state persist + tests).

Next: models 321-624, session_tracking, tracker.

## Entry 077 — session_tracking.rs (326 complete) + tracker.rs (714 complete)
- session_tracking OK: weights opus2/sonnet1/haiku0.5 word-boundary regex (no lookaround → (^|[^a-z])fam([^a-z]|$), OnceLock 4315x), config dir CLAUDE_CONFIG_DIR else ~/.claude, walk jsonl recursive, read 10MB cap (stat+read_exact else take fallback on shrink), usage i64 f64→i64, cache_creation 5m/1h + total fallback sum, window [start,end) parse skip bad, msg usage null skip, dedup message.id (19x 420K lesson, no-id keeps per-line), totals + by_model first-seen order + unattributed weight 1.0, weighted sum. Tests weights/transcript/dedup (msg_a once + b once). No bugs. Resolves WindowTokens weighted (Entry 075).
- tracker OK: pure state machine (async/fetcher in proxy, RTK subprocess not ported + pure delta preserved, explicit deltas only). new persist default paths::subscription + clamp poll 1-3600 + active max5 + load. notify_active Bearer case-insensitive (75105e23) skip sk-ant-api, hash+last4 + last_active + full_tokens prefix count (PR-F3 no raw persist). update deltas max0 (+ explicit Some0 honored). rtk delta regression→0 rebase. state/latest/is_active/poll_token disk source/current_token_id hash. render synth 5h/7d + opus/sonnet optional (transcript used_since_reset weighted else raw). on_demand floor once-per-window. poll_once token→fetch→window→discrep→add+window+discrep+clear err+reset contrib→persist; None token/fetch→false + mark_error. persist tmp.pid + pretty + rename; load raw contrib proxy_compression else compression (no double), cli raw else cli/rtk, legacy no rtk_raw→rtk mirror else rtk_raw, cache, usd, poll_count. token_id sha256:16hex…last4 whole-chars (3.27x byte idx, empty→… tail). reset on forward >1min jitter. window [reset-5h,reset). surge expected weighted/limit vs actual >15 → warning <30 else alert; cache input>50k && read<10% → warning. Tests 10: render cached, ignore api, lowercase bearer, raw not stored, unicode tail, explicit zero, rtk delta, poll persist (env token, leak tempdir forget), surge, legacy rtk. Unwrap poison (INCONSISTENCY-003). tempdir forget leak in tests only. No prod bugs.
- Subscription stack complete (mod/base/client/models/session/tracker).

Next: savings_ledger (870), savings_tracker (3319), output_savings (1150), perf_analyzer (1752), persistent_metrics (2049), etc.


## Entry 076 — subscription/models.rs 321-624 (completes file, 624 lines total)
OK: Contribution cli=max(filter,rtk deprecated alias) + total/compression/raw/eff + to_value dual rtk/cli keys + raws (cross-compat). Discrepancy kind/desc/severity/expected/actual/delta. State latest/window/contrib/discrep 20/history 100/polls/errors/last/active; add_snapshot cap drain oldest + poll++; mark_error; add_discrep cap; is_active elapsed≤window; to_value last5 discrep reverse-take-reverse (chrono order) + latest/window/contrib/polls; persist + last20 history. Tests 6: window parse/value, snapshot omits None + prefix8, cli max + raws, synth cached/past-cap/missing-resets. Sound. rtk alias matches cost SessionSummary rtk=cli compat (Entry 028). No bugs.

Next: session_tracking (326), tracker (714), savings_ledger (870), savings_tracker (3319), etc.

## Entry 078 — savings_ledger.rs (870 complete)
OK: append-only JSONL flock EX+SH, no shared state. Pricing vendored else blended 3/M (litellm-absent shape), schema/field/order/encoding match Python shared file. Helpers round half-even, parse Z/naive UTC, normalize unknown, sanitize percent-decode + printable (space ok, control/ws no) trim 128, unquote ASCII, label unknown fallback, path via paths::savings_events. estimate ≤0→0 else vendored input else fallback round6. record_from_forwarded reconstructs before=forwarded+saved (40% not 67% lesson, before-after==saved invariant, only place) + cost variant (cache placement, legacy None); ≤0→false. record event max0, saved=before-after ≤0→false, cost explicit max0 else estimate, ts RFC3339 micros +00:00 (Python fromisoformat), ordered map v/ts/before/after/saved/cost/basis/model/client/source/pid, write_locked mkdir+compact separators ,: + flock EX write Unlock, maybe_compact >8MB rewrite ≥cutoff 30d EX (malformed/drop-old, truncate+join\n). read shared lock, trim skip empty/malformed/no-ts/<cutoff (0→None no-cutoff, but aggregate clamps 0→30 so unbounded impossible; direct read_events 0 = no cutoff — internal only). Bucket add + pct 0 if before≤0 + to_value round6/1dp. Report to_value Python shape. ranked cost desc then saved (stable insertion for ties) + key first. OrderedBuckets insertion order (Python dict). aggregate clamps ≤0→30 + min(30), today local midnight→UTC + week 7d, per-event max0, model/client unknown fallback, top_model first row else unknown, lifetime=windowed 30d (no all-time, matches Python). coerce num/str else 0. Tests 13: reconstruct 40%, ignore non-saving, cost verbatim, retention cap 0/365/30, blended unknown, short-circuit, explicit, zero/neg skip, by-dimension, windows today/week/30 incl cutoff, 365 excludes 400d, durability + corrupt skip. Whole file sound: 40% lesson pinned, cap prevents unbounded, compact under EX, corrupt tolerant. No bugs. Used by outcome ledger (Entries 059-060) via record_from_forwarded_with_cost.

Next: savings_tracker (3319), output_savings (1150), perf_analyzer (1752), persistent_metrics (2049), etc.

## Entry 079 — savings_tracker.rs 1-280 (chunk 1/11)
Partial OK: SCHEMA4, 60-min rollover, 50 projects, 5000pts/365d, atomic tmp+fsync+rename (verify later), hourly/daily/weekly/monthly on demand. Vendored pricing blended fallback (litellm-absent shape), BTree deterministic (no byte parity needed). coerce max0 / non-finite→0 / signed finite else 0. normalize unknown sentinels. sanitize same as ledger (percent+printable+128). estimate compression/output ≤0→0 else vendored input/output rate (zero vs unknown distinguished — free spark 0 stays 0, not fallback $3/M; bugfix documented). cache_write split measured not assumed (force-1h 2.0x vs 5m 1.25x lesson; uncovered→5m cheaper understates not invents; overshoot split wins residual floor 0 no refund). input cost breakdown vs total, zero vs unknown (free stays 0). cache savings reads discount - writes premium (negative possible), unknown fail-open (no counterfactual). Continue 281+.

Next: savings_tracker 281-3319.

## Entry 080 — savings_tracker.rs 281-580 (chunk 2/11)
Partial OK: cache savings all_fresh - actual (reads discount, writes premium negative possible; unknown→0 fail-open no counterfactual). pct baseline = client-on-own (NOT all-fresh; counting full cache discount read 87% vs <1% lesson $1095 vs $164; left = removed tokens; actual cache-priced + compression basis adds once; stabilisation unmeasured excluded; offload would-did folded). Lifetime/output/offload/cache_reads serde defaults (old files load), FailedWork separate (failed must not improve success rate, durable books attempts/at_risk/usage/by_status), HistoryEntry cache defaults, State + history_rendered mirror (push/trim only, rebuild on load/replace; edit-in-place forbidden), projects BTree, metrics PersistentMetricsState home (else finished port no callsite, busts lost on restart). sync mirror len check. Tracker coalesced writes (9 record→save whole 1.48MB fsync holding lock →45/s pileup; now state per-record, disk ≤1/s + drop flush). RequestRecord neutrals + tool_schema additive (not compaction) + cost basis + output separate input/output + lifetime-only fields (outcome carried, tracker dropped → blob silent on cache). Continue 581+ (record_* methods).

Next: savings_tracker 581-3319.

## Entry 081 — savings_tracker.rs 581-880 (chunk 3/11)
Partial OK: FailedWorkRecord optional usage (never masquerade estimate as billed). new/with_options defaults + max1 floors + load. record_compression ≤0→false else lifetime saved/usd/total max() + history push/trim/save. record_request headline saturating + cost explicit max0 else estimate + tool separate + output OUTPUT rate separate (never mix input/output) + input/cache/offload deltas; lifetime max() next tokens/cost, session deltas max0, requests/saved/usd/output/offload/cache reads/savings; rollover none→true else >60min reset started; session pct actual+compression+offload (cache reported-only); project record; history gate headline>0|read>0|output>0 (cache-mode d1258055 blind fix); metrics record_request durable both sides (compression vs busts). Sound. Continue 881+.

Next: savings_tracker 881-3319.

## Entry 082 — savings_tracker.rs 881-1180 (chunk 4/11)
Partial OK: failed_work attempts max1 + saturating + usage optional (estimate≠billed) + by_status + ts. bust/miss/overhead/unbooked/tools passthrough to metrics + save (overhead skip if equal no lock/write). reports snapshot/verdict/wire passthrough. project sanitize else skip, max0 deltas, evict smallest/oldest ≠touched over 50 (tokens, ts tiebreak), snapshot sorted saved desc (BTree key order stable) + pct saved/(saved+input). display expired→empty else pct actual+comp+offload (cache reported). snapshot/history/preview recent tail. Sound. Continue 1181+ (history_response/save/load).

Next: savings_tracker 1181-3319.











## Entry 071 — mistral.rs 181-440 (completes file, 440 lines total)
OK: DEFAULT_REG 8 prefixes (5 families + large/small/nemo tekken 131k vs v1 32k longest-wins). try_register fail-soft per-prefix failures list, empty=all ok (gated repos fail Hub, pixtral check). Tests 11: version direct/prefix (open-mistral-nemo v1 quirk preserved)/default v3/case, repo direct/prefix-5-families/most-specific/case+foreign None, registration routing/longest/unregistered estimator 4.0, ignored network fail-soft. Parity absence deliberate (Entry 068). No bugs. Tokenizer stack complete (mod/estimator/registry/hf/tiktoken/mistral).

Next: savings_ledger (huge?) + savings_tracker (3319) chunked, subscription, tool_*, transforms/, ccr/, remaining core, then proxy/.







## Entry 065 — tokenizer/mod 201-572 (completes mod, 572 lines total)
OK: Strands reasoning/document/pages (bytes/3000 max1 *1500)/image 1600/video 3200/unknown serialized; str parts counted. count_serialized to_string else LARGE/4, ≤50k direct else sample 10x2k evenly scaled (char-boundary floor, empty→full, scale len ratio). coerce null/missing→"", str→s, obj/arr→json, scalar→to_string (object args priced not zeroed — malformed stays in history, undercount compounds). tool_calls 4/call + func name/args coerce + id coerce; function_call 4 + name/args. Tests 17: empty→reply, single/str/array/image/audio/tool/legacy/name/multi/tr/strands text/tool/reasoning + object-args counted≈string ±4 + null→4. Char-boundary + null/object pricing fixes documented. No bugs.

Next: tokenizer estimator/hf/mistral/registry/tiktoken, savings_ledger/tracker, subscription, tool_*, transforms/, ccr/.




## Entry 062 — rollout.rs 81-444 (completes file, 444 lines total)
OK: NativeBedrock + ResponsesStreaming stable-default-on, CanaryProbe canary-not-default. Reasons Default/Explicit/Legacy(disused?)/Disabled/Blocked/Unsafe/NotRequested. Config channel/requested/disabled/unsafe. Snapshot schema1/policy1 + registry digest + decisions; default stable/""/""/false. from_parts unknown channel→warn+stable, validated names warn+fail-closed intersect, explicit Features appended. decision expect (all registered), is_enabled ignores explicit bool param (underscore — dead param? Signature is_enabled(feature,_explicit) ignores second arg. Callers pass what? Possibly legacy. Flag MINOR-005: dead _explicit param, API noise, not bug). enabled set, eligible !unsafe, canonical schema/policy/channel/unsafe/digest/features, digest sha256:hex of serde vec (note: serde_json field order = struct order, deterministic; Python parity? vectors test pins), to_value + digest + eligible + ineligible reason. resolve: disabled wins over all (incl unsafe) → Disabled; requested+!avail+!unsafe→Blocked; +unsafe→UnsafeOverride true; requested→Explicit; default allows→Default; else NotRequested. validated warn unknown. split ;→, + normalize lower -→_. registry digest, names set, digest sha256:hex. Tests 8: order, blocked reason, default reason, unsafe ineligible, disable beats all, digests deterministic/policy-sensitive, invalid fail-closed, shared Python vectors via upstream-python fixtures (parity gate — verifies Python file exists later). Sound. MINOR-005 dead param. No bugs.

Next: savings_ledger/tracker, subscription, thinking_tokens, tokenizer, tool_*, transforms/, ccr/, etc.



## Entry 060 — request_outcome.rs 461-1165 (completes file, 1165 lines total)
OK: Sink extra record_output (shaper labels) / failed (>=500 generic or explicit forward incl. 4xx upstream) / ledger (only if saved>0 headline or rerouted; flocked disk off-path) / cache_outcome (ttl/prefix/unknown + wasted, turn-after detect, proxy types stay proxy). emit_failed status-agnostic + warn request_failed_accounting once (no success/PERF/savings; attempts max1, at_risk opt*attempts saturating, saved_not_booked naming). emit funnel: >=500→failed stop; 4xx normal (client errors served). Shaper hook first if output_shaper:. Novel via ledger lock poison-recover (matches conversation_savings) fallback saved; booked view for record_request/tokens, wire truth for ledger/log/PERF. Ledger gate headline(tags)+reroute (reroute books bill-never-sent; helper ignores zero). Order request→ledger→tokens→log→PERF info (tok_novel only if differs, tool/total headline, inflated, cache, opt/total/out/ttfb/transforms/client). Tests 25: cache_hit, basis read/fresh + headline count + clamp, hit/savings pct, stream derives/contents/gating, summarize, ledger order/skip, 5xx failed-only, novel 62806→13214 accum vs wire ledger, no-key fallback, explicit 4xx/5xx once, callsite Interest race fix (global registry + rebuild, 4% flake), 5xx log forwarded/not_booked, 4xx success funnel, order w/o + with shaper first, minimal noop, inflated clamp diagnostic. Whole file sound: failure can't inflate save-rate, novel once-per-conv, ledger gated, double-count guards. No bugs.

Next: rollout, savings_ledger/tracker, sqlite_tuning, subscription, thinking_tokens, tokenizer, tool_*, transforms/, ccr/, proxy/, etc.

## Entry 083 — worktree evidence + tracker-hygiene note + savings_tracker.rs 1481-1600 (chunk 5/11)
- EVIDENCE (2026-09-11, `git status --porcelain=v1 | wc -l` = 202 changed paths; `git diff --stat` tail = 107 files +6714/-12324; `VERIFICATION_AUDIT.md` untracked `??`). Agent made zero code edits this run (only Read/Grep/Glob/Bash-reads + edits to this tracker). The ~202 modifications (incl. `M Cargo.lock`, `D TODO_ARCHITECTURE.md`, `M contrib/*`, `M crates/headroom-core/src/retry.rs`, `M crates/headroom-proxy/*`, wiki/) are pre-existing worktree state, not audit activity. Consequence: all findings verify a DIRTY tree — line numbers/behaviour may differ from HEAD. No code was changed to fix anything per goal constraint.
- TRACKER HYGIENE (self-finding): entries 060/062/065/071 appear AFTER entry 082 in file order. Cause: earlier turns replaced mid-file `Next:` placeholder lines instead of appending at end. Content intact, numbering chronological by creation, file order scrambled. From here on, new entries append after this line (true end). Record only, no rewrite per append-only rule.
- savings_tracker 1481-1600 OK: sanitize_state lifetime coerce (requests/saved/usd/input/cost + output/offload/cache with serde-default backward compat — old files load not rejected), floors lifetime at last history cumulative (max(), prevents backward drift after load), round6 usd. State: failed_work/display/projects normalize, metrics `PersistentMetricsState::new(lifetime_metrics)` + `load_footprint` (older blob upgrades in place), trim_history on load. save(): stateless→skip; dirty flag; coalesce ≤1/s via last_write (poison-recover; 1.48MB whole-file fsync pileup fix from Entry 080). Sound. Continue 1601+ (write_state/load/save/flush).

Next: savings_tracker 1601-3319.

## Entry 084 — savings_tracker.rs 1601-1900 (chunk 6/11)
OK: flush() stateless/dirty-gated, poison-recover, last_write refresh; Drop flushes (crash can lose ≤1s, documented). write_state: stateless/parent/mkdir silent-skips; last_saved_at stamped pre-serialise, rolled back on serialise/write failure (snapshot never claims unsaved save); borrowed Payload (no history clone; field order = file key order, ex-json! parity); sync mirror; pretty JSON; atomic tmp `.proxy_savings_<nanos>.tmp` + write/flush/fsync/rename, tmp removed on err, dirty cleared on ok. Name collision: nanos timestamp could collide across processes in same ns — rename overwrites same tmp name; both writers fsync then rename, last wins, no interleave (acceptable; no O_EXCL). Value shapers (lifetime/failed/display/history/projects) mirror persist schema; normalize_failed_work coerces + by_status i64-only; history_entry legacy object (missing cache/output →0, bad ts→drop) + legacy array [ts,saved,usd,input,cost] form. Sound. Continue 1901+ (normalize tail + tests).

Next: savings_tracker 1901-3319.

## Entry 085 — savings_tracker.rs 1901-2200 (chunk 7/11)
OK: normalize tail rounds to 6dp; display requires both timestamps + last≥start else default (corrupt→fresh session, no panic); pct recomputed not trusted (stored pct could be stale-scale); projects re-sanitized (unusable names dropped — load can shrink count), bad ts→None ts, cap 50 by (saved, ts) desc. bucket_start hour/day/Monday-week/month with unwrap_or(ts) fallbacks; unknown bucket→ts (no crash). csv null→empty, strings/numbers escaped (comma/quote/NL/CR, "" doubling). RollupEntry totals + deltas + by_provider/model BTree (deterministic). Tests start. Sound. Note: normalize_projects on load re-caps to 50 while record_project evicts smallest — consistent policy both paths. Continue 2201+ (tests).

Next: savings_tracker 2201-3319.

## Entry 086 — savings_tracker.rs 2201-3319 (completes file, 3319 lines total)
OK: tests ~40 pinning regressions, all match code: cache restart survival + net -2100 "costing more than it saves" verdict honesty; reread_compressed persist + round-trip; overhead added/removed/net/per-req (negative net kept, hides-nothing); wire bytes 40% + billed 3000 + null-when-no-data (not zero); last_saved_at stamp + round-trip; other-bucket round-trip (loader vocab gap fixed, no unbounded `unknown`); shrink negative; tool inventory (sizes not doubled, worst first, drop-server suggestion chrome-only, fully-used empty, subagent-narrow survives); pre-metrics file upgrades; clean win; lifetime/session/history/projects write; cache-only history gate (d1258055); request-scoped price override 10x lesson; cache econ math (read discount/write premium/negative/unknown-0/1h-vs-5m/mixed/residual-5m/input-1h/restart persist/migrate-zero); discount-reported-not-counted 87%-vs-<1% lesson (47.62% math verified: 0.003/(0.0033+0.003)); reject nonpositive; rollover 90min→1 + lifetime 2; eviction p000; sanitize; round-trip schema 4; stateless no-write; rollups hourly delta 300; CSV header+1 row; preview shape; failed durable excluded (at_risk 143000 = 41000*3+10000*2) + metrics failed 2; schema shapes incl. 26.79% (300 saved @cache-read basis vs fresh — consistent with cost_basis boundary rule Entry 059); history/rollup shapes; output separate streams/rates/clamp/estimator; free-model zero-not-fallback (input + output companions, spark both spellings; unknown still fallback); offload 100% on free-serve; legacy load. Whole tracker: free-model + cache-placement + novel/counting lessons all pinned by tests. No bugs. File complete.

Next: output_savings (1150), perf_analyzer (1752), persistent_metrics (2049), remaining core (relevance impls, signals impls, ccr/, transforms/, tokenizer hf tail done, subscription done), then proxy/.

## Entry 087 — output_savings.rs (1150 complete)
OK: 3-tier honesty (estimated synthetic-control / measured A/B / echo direct; request-time stratification only). Deviation: stateless checks env only (no process global like Python). Buckets xs/s/m/l/xl 2k/8k/32k/128k; family opus/sonnet/haiku/fable/mythos/gpt/gemini else other (contains, lower); stratum family|kind|bucket|tools (specific→general for backoff); first 512 chars codepoints (Python slice parity); conv key model+NUL+first-user-text sha256 (stable across turns); arm sha256(arm:key)[..8]/2^32 vs fraction, ≤0 treatment / ≥1 control. Accum n/sum/sumsq mean/var-sample-max0/merge. Baseline observe strata+glob, merge, lookup exact→prefix-trim (first match — HashMap iteration order nondeterministic across prefixes! Multiple strata sharing prefix return arbitrary one. MINOR-006: backoff prefix scan over HashMap is nondeterministic when >1 sibling shares prefix; deterministic would need sorted keys. Low impact (estimate only), record). Modelled table ships EMPTY (dash not guess; holdout > benchmark; conservative headline). register validates (0,1) + cons≤opt, replace on re-register; test clear + mutex (parallel). Estimates: baseline signed deltas (chattier pulls down) var n*acc.var + n²*mu_var/m, holdout per-stratum both-arms else None, finalize 1.96se CI, best measured>estimated>modelled(level-gated; default no modelled). Model math saved=O*r/(1-r) not O*r (r=0.2 →0.25O; 800→200 not 160), band=benchmark spread not CI. Save atomic tmp pid+seq create_new + fsync + rename (torn-write → load-empty-reset lesson; create_new fails if tmp exists — pid+seq unique per process, cross-process pid reuse could collide but create_new errors → tmp removed → err returned, no silent reset. OK). Load missing/corrupt→empty+warn. Labels stratum:/control: encode/decode; stateless env non-empty. Recorder load-at-new + record first-label-only + flush_every + estimate read-only clamped0 (tier-1 signed vs rollup accumulate-only floor — documented non-tension; banker's round_ties_even). reload baseline if disk differs (learn --apply). get_recorder workspace/output_savings.json flush 25. echo_ratio word n-grams (short→0, empty ctx→0). Tests ~30: buckets/family/key/accum/backoff+global/arm extremes/distribution/conv-stable/label/holdout/both-arms/baseline-signed/roundtrip/flush/echo/python-parity values/read-only/half-even/hierarchy-fallback 40-not-0/torn-write/modelled empty/math/yield/reject. MINOR-006 nondeterministic backoff scan. No prod bugs (estimation-only).

Next: perf_analyzer (1752), persistent_metrics (2049), remaining core (relevance impls, signals impls, ccr/, transforms/), then proxy/.

## Entry 088 — perf_analyzer.rs (1752 complete)
OK: log-analysis port (proxy.log* + launcher ~/headroom-proxy.log / HEADROOM_PROXY_LOG_PATH); deviations vendored pricing (free 0 renders $0.00 not unknown — deliberate vs Python truthiness), no TOIN-highlights (Python renders without when backend absent — same shape), RTK-only context tools. Regexes PERF/STAGE/router/Transform/TOIN OnceLock expect-valid. get_list_price Some(0.0) for free (not None). parse_kv transforms-last-with-spaces (k=v parts extracted, rest joined; old comma format kept). PerfRecord + headline_saved/before (tool never in before — denominator moves; old lines tool=0 exact). Cutoff overflow→all-data (Python OverflowError parity), unparsable ts kept (fail-open), oldest/newest string-compare (format fixed-width, lexicographic = chrono). feed_line: STAGE_TIMINGS JSON stages f64-only (null dropped) by rid; PERF star-strip counts / comma-legacy / none→[]; router NUM_KIND `^(\d+)\s+(\w+)` + content-block gate; transform/toin lines. JSON tracing adapter (timestamp RFC3339→local legacy format, target/level defaults; non-PERF→None). collect proxy.log* mtime-sorted + active log (workspace-override → None, tests isolated) + dedup. parse oldest-first. pct 1dp guard, percentile lerp, throughput wall/active/compression(first_stage else compression)/forward/generation(ttfb-adjusted) p50/95 round2, rolling + 5m current (unparsable ts excluded from windows but included in rolling all — MINOR-007: rolling uses all records incl. unparsable-ts while current uses parsed subset; negligible, record). Recommendations thresholds (cw>1.5cr, early<0.5, slow>20%, excluded>3x compressed, TOIN 0% + >100, aligner<10). RTK subprocess gain json (lean-ctx None; key aliases; saved fallback in-out; pct fallback; PATH then managed bin). Report headline tool split messages/schemas, BTree by_model, cache unstable/first-last-5, overhead, throughput, convo size, transform sort -saved, router shares + dominate flag, TOIN + 0% flag, recs, cli before footer, footer files|lines|dir. Summary JSON headline + splits + cache + BTree models + transforms + throughput + cli. Tests ~20 incl. JSON tracing, star-strip, stage attach, window filter, router/transform/toin, percentile, throughput 200tok/s, empty/RTK/sections/unknown-price/recs/summary-shape/rotated-logs/active-log. No prod bugs (MINOR-007 negligible).

Next: persistent_metrics (2049), remaining core (relevance impls, signals impls, ccr/, transforms/), then proxy/.

## Entry 089 — persistent_metrics.rs (2049 complete)
OK: bounded Lifetime aggregate (providers 32 / stacks 64 / models 200→100 exposed + other / tools 400 + 15 listed / 7d retention / labels 128 chars; byte-exact to_dict vs Python, Rust-only footprint beside blob). Coercions mirror Python int() try (bool/str/int/float-trunc, neg/non-finite→0; u64 saturate; "12.5"→0). round6 via format-parse (CPython ties-even on binary value). CountMap insertion-ordered Vec (reassign keeps pos, removal shifts; compact smallest-first ties-by-label into `other`; observe_max for definition sizes — sum would multiply per-turn). ModelMap same + ranked (-observed, ts, name) + merge later-ts. Record: headline saturating (tool additive), labels capped, failed/rate/cached/requests, cache 5m/1h/uncached, cost round6 (cache signed — write-heavy can go negative), waste known else `other` (matches record; loader same — restart-stable, Entry 086 test), models other-fold for unknown. Bust/miss (known else unknown)/failed/rate/stack helpers stamp activity. Verdict saved-bust (prefix_change = our fault vs ttl time; unbooked holes beside number; free-subscription reads shape). Wire bytes vs provider usage (same-request sets only; null when no data, not zero; bytes-per-billed steady = converted). to_dict byte-exact (proxy_overhead/tool/wire skipped → footprint_to_dict beside); footprint load upgrades (pre-last_seen seeded now, else aged-out). normalize all-fields coerce + caps + unknown-model→other merge. snapshot read-time pcts (token_savings saved/input — fixed 30,891% lesson: attempted excludes cache on Anthropic, byte-identical uncached double-mean; attempted kept as separate active-savings denominator), null when empty, persistence last_saved_at overwritten from state (no false save claim). suggest_droppable whole servers only (built-ins riskier; empty not reassuring-empty). Tests ~25 Python-measured vectors: empty shape byte-exact, coerces, round6, labels, full record shape byte-exact, snapshot shape, 30891% regression (~68%), null pcts, corrupt→empty, unknown→other, other-bucket reload, miss fallback, compact eviction (other=7 not 5 — evict-per-increment), model prune top-100 + other.requests 101, helpers stamp, round-trip stable. No bugs. Used by savings_tracker metrics_* (Entries 080-086). Core metrics stack complete.

Next: remaining core (relevance bm25/embedding/hybrid/base, signals keyword/line/tiered, ccr/, transforms/ ~40 files, tokenizer done, cc_switch? no — core list: compression_policy done, conversation_savings done, cost_tracker done, ctx done, memory done, onnx done, output_savings done, parser done, paths done, perf_analyzer done, persistent_metrics done, pricing done, proxy done, relevance partial, request_outcome done, retry done, rollout done, savings_* done, session_sticky done, signals partial, sqlite done, subscription done, thinking done, tokenizer done, tool_* done, turn_id done, waste done) then proxy/.

## Entry 090 — relevance/* + signals/* impls (all complete)
- relevance/base (139) OK: clamp [0,1] (Python __post_init__), empty(), trait score + default batch + is_available, free fn. Tests 6. No bugs.
- relevance/bm25 (385) OK: UUID→4+digit→alnum order (parity, \b supported), k1 1.5 b 0.75 /10.0, idf ln2, sorted keys (membership-only), +0.3 len≥8, caps 10/5, batch amortized + avgdl + empty→empties. Tests 15. Sound.
- relevance/embedding (472) OK: bge-small-en-v1.5 same ONNX file (~1e-6); Default None-unavailable (cheap, no I/O); AVX2 guard #1723 + dynamic ORT preload + Mutex (inference-bound contention). Cosine clamp [0,1]. Non-ml stub identical empties. Tests offline + gated + guard. Sound.
- relevance/hybrid (364) OK: alpha*B+(1-a)*E, UUID 0.85/ids≥2 0.75/1 0.65/host|mail 0.6, [0.3,0.9]; fallback ≥0.3 +0.2 ≥2terms; Python email-typo `[A-Z|a-z]` mirrored (documented); cached availability; zero-change ONNX landing. Tests 14. No bugs.
- signals/line (84) OK: 4 ctx, 5 cats, priority vs confidence, const fns, Send+Sync. No logic.
- signals/keyword (433) OK: 2 Python bugs fixed + pinned (4 missing err kws in set; `token` dropped from security). conf 0.7 (≥0.7 escalate → wins by default; ML override headroom documented). Automata universal/warn-nonDiff/sec-Diff-only/lax-indicators; LeftmostLongest + ASCII word-boundary post-filter. MINOR-008: boundary ASCII-only vs Python Unicode \w (non-ASCII-adjacent edges may differ; low impact). Within-automaton first-position-wins = regex leftmost parity (not priority-ordered). Markdown Text-only. Tests 11. Sound.
- signals/tiered (141) PARTIAL (1-80 read): first ≥0.7 wins else best-guess; composition; with_detector sugar. Tail 81-141 (tests) NOT yet read — verify next before closing signals.
- Relevance complete. Signals pending tail only.

Next: tiered.rs 81-141 tail, ccr/ listing, transforms/ listing.

## Entry 091 — tiered tail (141 complete, signals CLOSED) + scope map
- tiered 81-141 OK: 4 tests (high short-circuits incl. over keyword; low falls to keyword Error; none-match returns best-seen Importance@0.5 not neutral; empty neutral). Semantics pinned. Signals module complete (mod/line/keyword/tiered + README still unread — 5 min later with transforms consumers).
- SCOPE (`wc -l`): ccr 7 paths ~6.5k (mod 149, batch_processor 599, batch_store 357, context_tracker 1094, response_handler 1862, tool_injection 1058, backends in_memory 360/mod 152/redis 214/sqlite 366); transforms ~57.7k across ~65 files (largest: content_router 4532, live_zone 4964, code_compressor 3242, crusher 2509, log 1803, diff 1781, content_detector 1480, search 1478, read_lifecycle 1416, compression_batches 1270, tag_protector 1272, anchor 1189, kompress_remote 1184, anchor...). Proxy 69 files next after core. Strategy: ccr first (smaller, load-bearing for retrieval loop + FINDING-009 context), then transforms alphabetical in chunks, then proxy handlers.
- No new findings (measurement only).

Next: ccr/mod.rs (149) + backends/mod (152) + backends/in_memory (360).

## Entry 092 — ccr/mod + backends/mod + in_memory + sqlite (all complete)
- mod OK: lossy-wire/lossless-end-to-end via hash-keyed originals; stripped to put/get (no BM25/feedback/metadata — runtime layer). Trait Send+Sync Arc-share; put overwrite-idempotent + bool gate for offload metrics; get None if missing/expired; len informational (Redis returns 0). DEFAULT_CAPACITY 1000 = Python; DEFAULT_TTL 1800s idle (5-min default outlived by sessions; expired = silent lossy). Idle sliding + 8x absolute cap (#2604). compute_key BLAKE3→24hex (96-bit, bounded LRU; `[..24]` on ASCII hex safe) matches Python regex `[a-f0-9]{24}` tool_injection.py:211 (verify later). marker `<<ccr:HASH>>` fixed. Tests key shape/det/diverge/marker. No bugs.
- backends/mod OK: InMemory test / SQLite prod / Redis opt-in; from_config loud errors (UnsupportedBackend if not compiled; sqlite/redis readiness checked) — no silent fallback (feedback_no_silent_fallbacks). sqlite_default 30min. Tests via backends. No bugs.
- in_memory OK: DashMap sharded + FIFO order Mutex (O(1) push/sweep; stale tolerated). Idle+ceiling expiry; lazy no-reaper. Re-store fast-path overwrites + resets BOTH clocks (matches sqlite created_at reset — consistent). Evict loop until len<cap (stale no-op). Race: get_mut-miss→insert prev Some→skip queue dup (no duplicates). get: get_mut check + touch, else None; expired→remove_if under shard lock (TOCTOU fix documented: old drop-then-remove wiped concurrent fresh put). Poison: `.expect` panics (INCONSISTENCY-003 addendum; same as cost_tracker). Tests 9 incl. concurrent 8x200 + TOCTOU regression (hits>100/200). MINOR-009: capacity 0 not floored to 1 (SessionStickyTracker floors; here cap 0 → evict loop drains then inserts anyway = unbounded). Config default 1000, low severity. Sound otherwise.
- sqlite OK: WAL+NORMAL (power-loss → 1 miss), PK upsert (idempotent, resets created_at like in-memory), legacy last_accessed migrate+backfill (keeps baseline, no purge/refresh), no secondary index (small table, PK-only + sweep predicate), Mutex (short/rare; N-files sharding documented; file locking multi-worker). Purge strict `<` (truncation extends <1s, never early-expires) + boundary tests (105 valid/111 gone; ceiling read-every-4s dies at 111; WSL2 CLOCK_REALTIME backward-step lesson → injected `now`, no sleeps). get_at: purge + lookup with both windows + touch under one mutex; warn-and-continue on purge/get/touch errors (loud, proxy keeps serving; put bool gates metrics per trait). len counts incl. expired (lazy, matches in-memory). pre-1970→0 no panic. UTF-8 row → None if binary (BLOB column, String::from_utf8 ok — non-UTF8 originals unretrievable; payloads are str on put path so consistent; note only). Poison expects (same addendum).
- DRIFT-006 (pending, recorded Entry 091 context): ctx/store.rs content_by_hash "CCR idle TTL of a week" vs ccr DEFAULT_TTL 1800s + 8x cap (4h max). Resolve via proxy CCR config wiring (which ttl_seconds prod passes) + flags --ctx-offload-ttl 604800 (different knob: gate staleness). Verify in proxy config/handler pass.
- No prod bugs (MINOR-009 low).

Next: ccr/backends/redis (214) + batch_store (357) + batch_processor (599).

## Entry 093 — ccr/backends/redis + batch_store (both complete)
- redis OK: key `prefix:hash` + born `prefix:hash:born` (ceiling), SETEX both on put (re-store resets ceiling — matches mem/sqlite), get→backfill legacy born with FULL max (generous, documented) → new_ttl=min(idle, remaining); 0 → DEL + None (ceiling enforced, not served). Non-atomic GET→TTL→EXPIRE: expiry between GET and EXPIRE serves just-expired payload once (benign direction; contrast mem TOCTOU wipe which was harmful). Errors warn + false/None (marker already embedded — graceful degrade per trait). Blocking conn per call (no tokio Multiplexed requirement — CCR called from sync+tokio). PING smoke at open (loud init). len()=0 documented (KEYS-scan unsafe; trait allows). Prefix avoids cross-proxy collision. TTL 0 → SETEX err → false+warn (loud-ish). No bugs.
- batch_store OK: BatchContext (id/provider/created/expires/requests/api_key/base) + Store TTL + max + oldest-10% evict (single pass; 5→4→insert 5, bounded; test ≤5). store() resets expires_at to STORE ttl (context ttl param redundant — harmless). get clones under lock + lazy expiry remove; remove/cleanup/stats straightforward. Poison recovers (good pattern). Tests 10 incl. sleep-based TTL (50/100ms — flaky-adjacent but generous margins), overwrite, provider stats. api_key in-memory by design (continuation calls; same as Python). No bugs.
- DRIFT-006 still pending (needs proxy CCR ttl wiring).

Next: ccr/batch_processor (599) + context_tracker (1094).

## Entry 094 — ccr/batch_processor.rs (599 complete)
OK: post-processor detects CCR calls in batch results; async HTTP stays in proxy. Config enabled/120s/3 rounds; handler max_retrieval_rounds ← max_continuation_rounds. get_custom_id google metadata.key else custom_id|id else "". extract_response per-provider + object-only (Python isinstance parity). analyze: disabled→[]; per-result custom_id→ctx (skip missing) → response (skip non-object) → has→parse (double parse, fine) → ContinuationRequest (msgs/tools/model/sys; api_key None caller-filled). update_result per-provider shape + anthropic type succeeded. google contents: parts-passthrough (assistant|model→model else user), system skipped (caller passes systemInstruction separately — verify proxy caller), str→parts text, array text/tool_result/functionResponse (name tool_use_id else CCR_TOOL_NAME; content as_str else "" — CHECK-002: non-string array content drops to ""; CCR flows are strings normally; verify), tool_use→functionCall, empty parts dropped, unknown blocks dropped. Tests 20: ids x4, extract x4, analyze disabled/no-ccr/ccr/missing/mixed, update x3, google x5, config, mixed. CHECK-002 pending (google tool_result non-string). NOTE: continuation_timeout_secs stored but unused in core (proxy may read; verify at proxy callsite). No bugs proven.

Next: ccr/context_tracker (1094).

## Entry 095 — ccr/context_tracker.rs (1094 complete)
OK: compact-summary guard (narrow 3-pattern + negative tests; stale re-add prevention). Config default 512/0.3/300s/proactive-true/max2 (512 sized vs 91-peak/100-thrash lesson, 1.3MB; first-sight-wins fixed permanent-youth + turn-200-announce-1 bugs). Keywords lowercased + stop + len≥2 (main.py path tokens kept). track: disabled→skip; first (turn,ts) wins, content refreshes; move-to-back + FIFO evict oldest + info log (turn_order.remove(0) O(n) @512 fine). analyze: disabled|!proactive→[]; empty workspace fail-closed; turn update; per-entry workspace→expanded-skip (snapshot contradiction note: re-expanding edited file contradicts disk) → age 300s → relevance (sample 0.5 overlap + 0.2/substr≥4 each + ctx 0.3 + fileop 0.1, cap 1) → age discount to 0.5x → ≥0.3 → sort desc → truncate 2 → mark expanded. format XML wrapper + workspace label + close-tag escape (backslash breaks tag match — effective). clear resets incl. expanded. Tests ~35 incl. first-sight turn-1 pin, no-twice, clear-reoffer, workspace isolation, threshold, age sleep 1/5ms, relevance x3, format x4, lifecycle x4, compact ±. CHECK-003: expanded marked at RECOMMEND time (line 319-321), not at use — if caller drops/filters recs, content never re-offered (lost until clear). Verify caller (live_zone/proxy) expands all recs; else lost-content bug. Config default proactive-true vs flags --ccr-proactive-expansion false = operator override (layered, consistent). No proven bugs.

Next: ccr/tool_injection (1058) + response_handler (1862).

## Entry 096 — ccr/tool_injection.rs (1058 complete)
OK: 5 marker patterns (standard `N word compressed to M` / legacy / generic case-insensitive / smartcrusher `<<ccr:12-24>>` + suffix / retrieve_more bare phrase for CodeCompressor `N tokens compressed...hash.H Expires` upstream 12a26b8f — each gap documented with upstream ref). Tool defs per-provider (anthropic input_schema / openai function / google parameters; hash+query, neither required = exactly-one). System instructions 5-hash cap + "...". Scan text/content-blocks/tool_result str+array/Google parts+functionResponse str+dict-values; order-preserving dedup (double: per-pattern contains + final retain — fine). Parse: shared name_and_input (anthropic/openai-string-args/responses-flat/google/default name+input|args); raw_ccr_hash (malformed Some("") vs non-CCR None — orphan tool_use resolution design); strict 12|24 hex + warn + loggable cap-64 control-stripped; query ≤2000 chars else None (unbounded-paste guard). Sticky-on (markers→inject+hashes; else session-flag→inject-empty (cache stability); else no). Tests ~40 incl. expires-suffix, suffix-marker, e2e x3, sticky x3. MINOR-010: openai(/responses) arguments assumes JSON string; object form → "{}" → hash lost → malformed path (Some("")). Parser.rs handles both; low risk (wire is string). MINOR-011: google description shorter (omits hash/query guidance + exactly-one rule); model may misuse query vs hash on Vertex. Low. No prod bugs.

Next: ccr/response_handler (1862).

## Entry 097 — ccr/response_handler.rs (1862 complete, CCR module CLOSED)
OK: residual 3-way (resolved / skipped_mixed #839 intentional hand-back / error) + parse keeps malformed on CCR side (25-char hash test; missing-hash; overlong query — all answered with error result, never orphaned to client). Extract per-provider (anthropic blocks / openai choices[0] / responses output[] / google candidates[0]; unknown → []). has_* triple-name check (provider-agnostic). IDs: anthropic id / openai id / responses call_id|id / google NAME (no id in Google shape). Splice-as-text for mixed turns (anthropic block→text, openai remove-call+append (str/null/array-preserving), responses→message item; google unsupported→0 hand-back pinned). Results per-provider shapes + sentinel keys (_openai_tool_results / _responses arrays; continuation extends). Assistant extract verbatim (responses full output[] echo). Streaming: buffer byte-window pre-check + SSE parse (data: lines, [DONE]/empty skip) + anthropic reconstruct (text/tool/thinking-verbatim+signature/redacted; bad partial JSON → {} input, downstream malformed path) + openai reconstruct (content concat + tool_calls by index, args string-concat). Tests ~60 incl. thinking-signature preservation (signed-byte lesson), redacted, tool paths, residual Python parity, splice x8. MINOR-012: google tool_call_id = tool NAME → multi-CCR-call Google turn resolves all to first result (no per-call id in shape; rare). MINOR-013: stream pre-check needs compact `"type":"tool_use"` (pretty-printed SSE evades; final parse proper; Anthropic wire is compact). No prod bugs. CCR complete (mod/backends/batch/context/injection/response).
DRIFT-006 + CHECK-002 + CHECK-003 still open (need proxy wiring pass).

Next: transforms/ — mod (123) + base (121) + detection (274) + config_compressor (415) + compressor_registry (450).

## Entry 098 — transforms/mod + base + detection (all complete)
- mod OK: parity-bound principle (must drop what Python drops; fixtures lock), Stats sidecars for OTel, ml-gated kompress/magika. Re-exports match files. No logic.
- base MIXED — FINDING-010 (staged, currently unwired): `split_frozen(messages, n)` with `n >= len` returns `([], all)` — nothing frozen. Per compute_frozen_count contract (N = exclusive floor; marker on last message → N == len → ALL frozen), correct is `(all, [])`. The `== len` arm inverts cache protection at exactly the all-frozen boundary (pinned by `split_frozen_all` test expecting ([], all)). Impact today: NIL — grep shows zero callers outside own tests; Transform trait explicitly unwired (same staged group as compressor_registry + content_router::apply_strategy; subscription arithmetic rationale matches DRIFT-002 dead flags). MUST fix before wiring (else cached prefix gets compressed when fully marked). Record, no code change.
- detection OK: Tier1 magika (non-Plain→return; err→warn + fallthrough, never hard-fail; AVX2 same bucket) → Tier2 unidiff → PlainText. Empty→PlainText without touching tiers. No-ml build skips Tier1 (= PlainText path). SearchResults/BuildOutput → PlainText deliberately (locked 2026-04-25; benchmark-before-detector). Tests branch on magika_available() with BOTH arms asserting concrete values (no vacuous pass) + naked-hunk Tier2 + determinism. Sound.
- No other bugs (FINDING-010 staged only).

Next: transforms/config_compressor (415) + compressor_registry (450) + cache_aligner (603).

## Entry 099 — config_compressor + compressor_registry (both complete)
- config_compressor OK: 3 tiers resolved by size (T1 lossless self-verifying round-trip; T2 comment/blank elision behind CCR; T3 TOML [[...]] → SmartCrusher csv-schema). Lossy never emitted unless stored (store None → tier skipped; marker unhonourable never emitted — pinned). Persist-FIRST (store before strip). INI column-0-only + keep-blanks (configparser multiline values); YAML/TOML whole-line + blanks; block-scalar/multiline → whole tier off (over-broad deliberately: false neg cheap, false pos deletes data). Schema_fold computed first, adopted only if strictly beats text tiers + own min gate; TOML strict-parse else None; crush passthrough → None. min_savings vs original (marker-cost economics: small comments declined, pinned). Tests 7 Python-verified values. Sound. Perf note (non-finding): SmartCrusher builder constructed per schema_fold call — fine at CCR rarity.
- compressor_registry OK: explicit registration (no entry-point discovery — genuine Rust capability difference, documented); pure-data boundary (strings/ints/bools only); opt-in selection (None/empty→none + info log; "*"→all; missing→warn+skip); BTreeMap sorted (Python sorted() parity); compressed default true (Python dataclass parity). Unwired deliberately + names only intended caller (content_router::apply_strategy) — staged group with base::Transform (FINDING-010 context, DRIFT-002). Tests 11 incl. opt-in safety property. No bugs.
- No prod bugs.

Next: transforms/cache_aligner (603) + cold_prefix (836) + safety (215).

## Entry 100 — cache_aligner + safety + cold_prefix (all complete)
- cache_aligner OK: detector-only (deep-copy, never mutates; PR-A2/P2-23). Policy override param (compression_policy Subscription-off wiring point; default config off). Volatile shapes UUID strict 8-4-4-4-12 / ISO YYYY-MM-DD-prefix (over-broad by design, warn-only) / JWT 3x≥4 base64url / hex 32-40-64. Byte-slice truncation safe (only ASCII shapes classified). should_apply/apply/score only STRING system content — MINOR-014: block-form system arrays invisible (should_apply false, no warnings, hash str-only). Verify proxy normalizes system upstream; low-medium. State external (caller-owned; Python per-instance _previous_prefix_hash bug class avoided by design). Frozen skip by index. Tests 19 incl. policy override both directions, immutability, metrics, frozen-skip. Sound.
- safety OK: tool-pair atomicity (assistant tool_use ↔ response) for live-zone co-decision; OpenAI id + Anthropic tool_use_id shapes; dup ids last-wins; unmatched dropped (caller leaves); pair dedup message-level (2 results/1 msg → 1 pair — decision unit). Tests 5. No bugs.
- cold_prefix OK: cold = safe-rewrite moment (cache dead); plaintext reasoning (Kimi reasoning_content / <think> string-only, unterminated excluded, block-lists = encrypted shape) dropped outright; encrypted (signed/redacted) never touched; cold turn re-caches smaller. Fail-open everywhere (Python never-raises). is_cold strict > ttl+margin 60s, NaN warm, no-TTL-source false. TTL from request cache_control (1h wins; msg-level + system scanned; numeric ttl ignored isinstance-parity) else env OFF > env hints > 300 default (never infer off — false-off recompacts warm every turn). should_cold_recompact None-TTL→true / None-idle→false. Spark summaries unsigned-only (Responses reasoning + unsigned thinking; signed/redacted preserved — provider-signed lesson); strip clears text slots + drops unsigned, never empties msg. recompact = lifecycle(frozen 0) + whole-conv dedup (ContentRouter folds/tags divergence documented; empty-guard). Env truthy sets differ by design (TRUTHY has `on`, recompact 1/true/yes — upstream parity, documented). Tests ~15 Python-measured incl. 400-vs-300 1h bug (hardcoded guess would bust warm 1h), gate truth table, TTL matrix, recompact fold, strip x3, decision matrix. Sound.
- No prod bugs (MINOR-014 block-form system).

Next: transforms/cross_turn_dedup (969) + compression_units (1064) + compression_batches (1270).

## Entry 101 — cross_turn_dedup.rs (969 complete)
OK: prefix-monotonic (earlier-only matching, absolute ordinals; is_prefix_monotonic asserts) + keep-earliest/verbatim-corpus (pointer always names physically-present original; folded spans None-break contiguity, never indexed). Floors 3 lines/40 chars (pointer ~35 chars; 4.3→6% lossless lesson); anchors cap 16/line first-seen (hot `return` bounded); trivial list (short/common closers). Line-number-tolerant: match_key strips leading `N:`/`N\t` (no-zero rule = Python `[1-9]\d*`), uniform delta carried (+5L/-3L) else run ends at divergence; non-numbered exact. Pointer `[↑NL same as msg T ±DL: 'anchor≤20chars']` (char-safe truncation; in-context recovery, no hash= token). Protected (frozen idx / cache_control) target-only. dedup_messages Anthropic tool_result-str + OpenAI tool-string, empty skip, <2 blocks noop, write-back only-changed unprotected, in-place only if folds. Tests ~20 incl. reconstruct-proof (anchor-locate + shift replays original incl. deltas), monotonic (block + router prefix-stability byte-equal), floor boundary 2-vs-3 + byte-exact pointer strings, non-uniform tail, frozen/cache_control/OpenAI shapes. DRIFT-005 addendum: py_repr 4th single-quote renderer (parser x2 + turn_id + here). CHECK-004: is_prefix_monotonic_with is O(n²) full re-runs per k — confirm tests/observability-only, not hot path. No prod bugs.

Next: transforms/compression_units (1064) + compression_batches (1270).

## Entry 102 — compression_units.rs (1064 complete)
OK: provider-neutral slot model (adapter extracts live-zone text, router compresses, adapter splices; slot opaque). Guards immutable > user-opt-in > system/developer > assistant-opt-in > cache_zone(live only) > byte floor — immutable wins pinned. Marker path: line-regex markers byte-preserved, segments compressed around them; already_compressed / rejected_not_smaller (token-gated via caller tokenizer, not word estimates) / applied. Standard path same gates + lossy-unmarked shell guard (Kompress/Text/CodeAware on local_shell_call_output structured output must carry marker, else verbatim). Transforms tags router:p/e/i:strategy + strategy. Reasons categorized (cache_zone_* dynamic). Tests ~30 incl. Python ports (user opt-in/out, immutable-wins, floor/zone, batch slots, marker surround, lossy verbatim vs recoverable). MINOR-015: marker-split segments re-gated at FULL min_bytes (600B unit → 300+300 both skipped; conservative/safe direction). CHECK-005: rejected branches carry the LARGER replacement in `.compressed` with modified=false — callers must honor `modified` (verify splice sites in live_zone/adapters). No proven bugs.

Next: transforms/compression_batches (1270) + compression_summary (607).

## Entry 103 — compression_batches.rs (1270 complete)
OK: sub-floor units grouped (nonce envelope + CCR placeholders + tag-protector), all-or-nothing split. Bounds validated (messages match Python ValueError); over-ceiling arm dead under valid bounds (pinned + commented). Grouping greedy order-preserving (compat key incl. f64 bias exact-equality = Python tuple parity; ≥min skipped+flushes; >max skipped not singleton; count/byte eager flush; under-floor tail skipped). Nonce sha256 entry_id+NUL+text+NUL [..12] byte-pinned ("3ba54ee2e42e"). Envelope positional strict (leading ws, order, trailing ws-only; C0 \x1c-1f note). CCR placeholders indexed (migration detectable per-entry). Split guards: empty/unchanged→no-change; placeholders ×1 (tag then CCR) else BATCH_INVALID; misplaced→INVALID; restore markers; per-entry shell-guard + size gates (same rules as units). Reason_category RAW (Python parity, differs from units bucketing — each matches its Python side). Tests ~30 Python-measured (nonce/envelope/placeholders/parse rejects/split applied/no-change/empty/garbage/not-smaller/ccr kept+dropped/shell lossy+search). MINOR-016: `batch.entries[0]` panics on empty batch (build never emits empty; public fn unguarded). Low. No prod bugs.

Next: transforms/compression_summary (607) + content_detector (1480).

## Entry 104 — compression_summary.rs (607 complete)
OK: dropped-item narratives (categories by type/status/... fields + notable error matches + shape fallback) + code-body names (6 shown +N more) + 4-regex name extractor (py/js/go/rust/java/class). Guards (empty/kept≥all→""). Tests ~30 incl. length cap <500, URL exclusion, fallback path. MINOR-017: no-indices fallback keys on FIRST 4 k=v pairs (distinct items colliding on first-4 counted as kept → summary undercounts dropped; summary-text only, not compression). CHECK-006: category tie order = HashMap random (sorted by count only) → summary bytes nondeterministic on ties; verify text lands in logs vs cache-key bytes (if forwarded, busts cache). No prod bugs proven.

Next: transforms/content_detector (1480) + unidiff_detector (322).

## Entry 105 — content_detector.rs (1480 complete, retired-oracle role confirmed)
OK: dispatch JSON(1.0 dict / 0.8 scalar / concatenated #1741) → diff(0.5+0.2h+0.05c, ≥0.7, 500-line window widened from 50) → HTML(doctype .5+html .3+head/body .1+struct .03 cap .3, ≥0.7, 3KB char-safe sample) → search(≥2 matches + ≥30% + path-shape no-</= guard, Copilot 2026-08-23 prompt-deletion incident pinned) → log(10 patterns error-weighted, ≥10% ratio) → tabular(md table 0.95 / delimited consistency per-delim + prose guard) → config(TOML strict-parse / INI reimplemented safe-direction / YAML heuristic + prose/front-matter guards) → code(first-match insertion order + first-on-tie, ≥3 hits) → text 0.5. Fixtures lock parity; Python-measured confidences pinned (php 0.6333, toml/ini 0.95, yaml 0.9). DRIFT-007 (documented in-test): single JSON OBJECT → Rust PlainText vs Python json_array/is_object:true; pre-existing, unrelated to config, deliberately unasserted. MINOR-018: dispatch-order doc comment (lines 232-243) omits Tabular/StructuredConfig steps present in code. Safe-direction divergences everywhere (INI→YAML/text, search→verbatim, grep→PlainText/SourceCode). No prod bugs (oracle role; production dispatch = detection.rs magika+unidiff).

Next: transforms/unidiff_detector (322) + magika_detector (736).

## Entry 106 — unidiff_detector.rs (322 complete)
OK: Tier-2 grammar oracle (retires regex per locked arch). Pre-gate ---/+++/@@ sequence (shell `+++ test.sh` rejected before parser) + PatchSet non-empty + ≥1 hunk (Ok(())-on-plaintext guard — honest contract). catch_unwind around unidiff 0.4.0 orphan-+++ unwrap panic (documented lib.rs:665; consistent with no-abort policy). Gaps documented: @@@ merge, CRLF, truncation canary (either-or accepted today). Accepted-gap note: headerless bare-@@ hunks (no ---/+++ at all) fail the pre-gate → PlainText (conservative/safe direction; "naked" test still has file headers). Tests 17 incl. panic regression, xtrace, added/removed-file, prose/json/code/html/yaml negatives. Sound.

Next: transforms/magika_detector (736) + anchor_selector (1189).

## Entry 107 — magika_detector.rs (736 complete)
OK: Tier-1 ONNX classifier, singleton Mutex<Result> (failure cached, no retry; poison → Poisoned deliberate no-recover). Loud errors (Init/Inference), never silent PlainText (audit-doc rule). Mapping explicit arms (json→JsonArray sans array/object refinement — PR5 router concern; diff/html/code-list incl. yaml/toml/ini/hcl/jinja; markdown/rst/latex/txt/empty/unknown + default → PlainText passthrough). AVX2 gate (shared onnx_cpu) + dynamic ORT preload (OnceLock deadlock lesson) + 5s side-thread init timeout (WinML System32 deadlock #1715; orphan thread leak-once documented; env override). Discovery venv/conda/cwd/pyenv/HOME/.local/site-packages, dedup existing. Tests degrade-gracefully both arms (no vacuous skips) + singleton reuse + table pins. CHECK-007: magika yaml/toml/ini → SourceCode while oracle → StructuredConfig and non-ml → PlainText; verify PR5 router reconciles config routing across tiers. No bugs in file.

Next: transforms/anchor_selector (1189) + adaptive_sizer (693).

## Entry 108 — anchor_selector.rs (1189 complete)
OK: pattern/strategy/weights (search-front .75/.15, logs .15/.75, balanced .45/.1/.45, distributed .5/.1/.4; recency/historical ±0.15 cancel; normalize, zero→default). Density 0.4 rarity + 0.3 length + 0.3 structural (Python weights/line refs). Budget int(max*pct) min/max/array clamps. Regions front [0,min(2F,N/3)) / back [max(N-2B,2N/3),N) / middle [actual-front, N-actual-back] (dedup-aware, mirrored). select_region even-spacing + adjacent retry / density top-N (stable ties). Dedup MD5[:16] first-4-key... (full-object hash; non-objects never dedup). Tests ~35 incl. Python hash vectors (8aacdb17, 6761da28), café/surrogate escapes, budget floors, density ordering. DRIFT-005 EVIDENCE+: unified JsonFmt renderer lives HERE (sort/compact/ensure_ascii 3-mode — the good copy); 0x7F DEL handling differs across copies (parser raw-ASCII vs anchor/turn_id escaped vs Python escaped) — concrete divergence proof, DEL-only impact. CHECK-008: length_score uses byte len (serde to_string().len()) vs probable Python char len — CJK parity gap? verify Python. No prod bugs (ASCII/CJK-adjacent notes only).

Next: transforms/adaptive_sizer (693) + recommendations (343) + relevance_split (388).

## Entry 109 — adaptive_sizer.rs (693 complete)
OK: 3-tier (n≤8 all clamped-to-max fix pinned; ≤3 simhash groups; Kneedle bigram coverage + diversity floor/ceiling; zlib 15% +20% bump). Parity documented per-fn (MD5 BE u64, codepoint grams, single-word ("",...)/empty ("","") cardinalities, strict <0.05, flat→1 literal, zlib level 1 miniz-vs-libz tolerance). CJK char-bigrams (curve was 1/item before). Bias int() truncation + zero-trap pinned (DEFAULT_BIAS≠0 cross-ref to live_zone — verify). Tests ~35 Python vectors (simhash hex incl. café UTF-8-bytes, knee 3/None/1, zlib bump 5→6 + representative passthrough, max/min/bias clamps). CHECK-008 extended: `full_text.len()<200` + length_score byte lens vs Python char lens (ASCII-identical; multibyte edge). No prod bugs.

Next: transforms/recommendations (343) + relevance_split (388) + observability (208).

## Entry 110 — recommendations + relevance_split + observability (all complete)
- recommendations OK: startup TOML (TOIN publish → ship → OnceLock → lookup (auth,family,hash)); dispatcher NOT consuming yet (PR-F3 pending, stated). Missing→info+empty, malformed→warn+empty (grep-able event; loud-degraded per policy). Dup keys last-wins silent (low; publish pipeline owns uniqueness). Tests 6 incl. cross-module AuthMode strings. No bugs.
- relevance_split OK: query prompt+tool-call composition; lossless segmentation (blank-delimited + windowed dense + indented-continuation; \r/\r\n/\n parity; tests pin join==content); Otsu cut floored; single/empty/over-cap → KEEP-all; runs coalesced. NaN-impossible (clamped scores) so unwraps safe. CHECK-008 addendum: block_chars/max_chars byte lens vs Python char lens (multibyte edge). Tests 13. Sound.
- observability PARTIAL-CONCERN: trait sound (cheap, no-fallback-None, no-batch, string strategy, must-not-raise). MINOR-019: MetricsObserver.record_compression is an EMPTY no-op (admits &self prevents counters; real counting in proxy PrometheusMetrics) + TestObserver placeholder no-op — names promise more than they do; verify no caller depends on them. CHECK-009: ExplodingObserver exists to prove failures don't propagate — verify call sites catch_unwind (content_router/live_zone pass needed). Size-gate global hook (OnceLock first-wins; silent no-hook no-op keeps core usable; returns installed-bool). Tests pin no-hook silence.
- No proven prod bugs.

Next: transforms/read_lifecycle (1416) + read_maturation (941) + read_protection (362).

## Entry 111 — read_lifecycle.rs (1416 complete)
OK: event-sourced Fresh/Stale(edit-after)/Superseded(covering re-read; stale wins; superseded default-OFF cache-bust rationale) + frozen demote-to-Fresh (counts too, pinned) + 512B floor + CCR persist-first (store None → still replaces with marker? Code: put attempted only if store Some; marker emitted regardless — marker unhonourable if store write FAILED (warns) or store None! Test elision_* in config_compressor required store; HERE replace_content emits marker even with store=None (tests pass None and assert marker). DIVERGENCE vs config_compressor rule (no lossy without recovery). Hmm: with store=None the hash is computed but nothing stored → model retrieves → miss. Is store always Some in prod? cold_recompact passes compression_store through (may be None). Record CHECK-011: lifecycle emits unrecoverable markers when store None/put-fails (config_compressor gates, lifecycle doesn't); verify prod always wires store. Range-cover logic (full covers all; partial containment; default limit 2000). OpenAI string-args / Anthropic object asymmetry (same class as MINOR-010; object-args OpenAI → unclassified, safe direction). Bash-edit blindness (sed/heredoc via Bash never stales; parity-bound presumably — verify Python list). Tag colon-path splitn documented + header collapse consistent. CHECK-010: "Retrieve original" lifecycle markers MISSED by tool_injection 5-pattern scan (no "compressed"/"Retrieve more" text; only compression_units regex covers it) → injection hash list may omit lifecycle hashes (model still reads hash in-text). Tests ~30. No proven bugs (2 checks).

Next: transforms/read_maturation (941) + read_protection (362) + recommendations done + relevance_split done + observability done.

## Entry 112 — read_maturation + read_protection (both complete)
- maturation OK: hold-quiet-reads-out-of-cache (never cache-written → no bust) + quiesce(5)/max-hold(25)/2KB floor; per-file clocks in assistant-turns; matured replay deterministic (content-hash marker; state-loss safe per test); lifecycle-marker passthrough (no double-compress); frozen untouched; breakpoint relocation preserves whole marker incl. ttl:1h (bare-ephemeral downgrade lesson pinned) + noop paths + input-immutability. MINOR-021: earliest-hold-idx 0 (or no prior block msg) → stripped breakpoint never re-anchored (lost); unreachable in practice (idx0 = user prompt, holds are tool results). MINOR-022: doc comment "Enabled by default while validated in pilots" vs code `enabled: false` — stale comment. No prod bugs.
- protection OK: SWE-bench turn-inflation lesson (re-read after lossy cat); 2 gates (command is-read + content not-confidently-data); off-by-default flag-gated. Wrappers/nested-shell/cd-prefix seen through (bash_program shared); writes/heredoc/tee excluded; lockfiles by-name carve-out (regenerated, never patched); OpenAI/Codex/Anthropic arg shapes; content protects Ruby/C/SQL (non-detector langs) + config, releases JSON/search/build/diff/html/tabular; empty→false. FINDING-011 (medium-low): THREE env-truthiness conventions — cold {1,true,yes} vs env_truthy {+on} vs protect_reads NOT-{0,"",false,no} (so `HEADROOM_PROTECT_READS=off` silently ENABLES). Real usability trap. MINOR-020: `sed -ne` combined flags missed (only bare `-n` token). Tests 12. No other bugs.

Next: transforms/log_compressor (1803) + diff_compressor (1781).

## Entry 113 — log_compressor.rs (1803 complete)
OK: 10-50x log pipeline (format → classify → stack-state-machine → score → adaptive cap → category select → context → CCR). 3 Python bugs fixed + pinned (chained-blank termination, conservative dedupe prefix-preserving, loud CCR). Format aho per-format one-hit-per-line, strict-greater ties → pytest-first order. Levels aho LeftmostLongest (warning>warn same-start) + ASCII boundary. 7 trace flavors incl. GoPanic/DotNet split (Java `at` overlap ordered first) + per-flavor terminators + cap-hit continuation (goroutine/source-echo coherence) + chained re-open on terminating line. Collapse runtime frames (head 3 + app 5, chain-heads always, marker score 0.8 anti-cap, continuations drop with frame, context-pass exclusion). Selection errors/fails first+last + score fill, warnings deduped (prefix-preserving), stacks ≤3, summaries all, context ±3, global adaptive cap score-desc. CCR MD5-24hex + ratio<0.5 + put-fail warns-AND-emits (same convention as lifecycle; config_compressor is the strict outlier — CHECK-011 reframed: codebase convention = warn-and-emit; verify prod store reliability, not a code bug). min_lines 50 verbatim path (format Generic). Tests ~30 incl. collapse-beats-truncation, chain-head survival, dedupe distinctness. Sound. No prod bugs.

Next: transforms/diff_compressor (1781) + search_compressor (1478).

## Entry 114 — diff_compressor.rs (1781 complete)
OK: parse (git/combined/cc headers, pre-diff preserved, rename/mode/binary captured, @@@@ hand-rolled alternation, 5+@ octopus → warning not silent) → file cap (changes-desc, names in stats) → hunk cap (first+last+top-middle, +N resort) → context trim (change±2, `\` markers always, no-change hunks head-only) → footer + CCR marker (MD5-24, >20% gate, count-before-append parity-by-1) + store persist (dangling-marker regression fixed + pinned). Lossy emits surfaced not silent (mode 100755→100644, Binary-filenames→bare). Scoring constants pinned; CJK bigram boost. Tests ~25 incl. bug-fix pins (rename/combined/no-newline/pre-diff) + gap tests (combined/cc headers) + fixtures shape (177→129). INCONSISTENCY-005: priority security set keeps `token` while signals/keyword_detector dropped it for LLM-token noise (different context — diff hunks vs metric lines — likely benign, undocumented). `let _ = n` leftover (harmless). No prod bugs.

Next: transforms/search_compressor (1478) + html_extractor (613).

## Entry 115 — search_compressor.rs (1478 complete)
OK: grep/rg parser (Windows drive, dash-filenames, 3-tier colon/dash/permissive with documented ambiguous tradeoffs + version-dot guard + whitespace-bounded colon tier + digit-path + adjacent-separator rejection), raw-preservation (item-16 zero-pad lesson: render from raw, grouped strips file-prefix at ASCII boundary), scoring (context overlap + keyword Search + config keywords + CJK), selection (file-score sort, per-file first/last + score fill, O(n log n) dedup, global adaptive cap, line-order restore), grouped rg-heading mode, CCR gates + warn-and-emit. Tests ~40 incl. incident regressions (Copilot datetime, zero-pad counterfactual, dated/CVE paths, Makefile ambiguity, body-hijack). Sound. Hash (SipHash) in-run only. No prod bugs.

Next: transforms/html_extractor (613) + lossless_compaction (1050).

## Entry 116 — html_extractor.rs (613 complete)
OK: functional (NOT byte) parity honestly scoped (dom_smoothie vs trafilatura; no-op knobs listed; categories/tags null-key parity; char-count math; is_html_content exact incl. 2000-char window). Metadata-before-parse (DOM mutation order), URL retry (relative tolerated), failure→"" (Python None parity), garbage-no-panic, batch order. Tests ~25. CHECK-012: failure yields extracted "" + ratio 0.0 (= reduction 100%) — verify caller falls back to original instead of forwarding empty (content loss vs savings confusion). No proven bugs.

Next: transforms/lossless_compaction (1050) + kompress (913).

## Entry 117 — lossless_compaction.rs (1050 complete)
OK: no-CCR reversible folds (ANSI strip one-way + run collapse + block back-refs + grep/file + grep/dir + path + diff-index-strip), every fold exact-inverse self-verified at runtime (mismatch/smaller-false → original; never panics). Timestamp-row exclusion (round-trip-invisible clock hoist — same lesson as search parser; both functors guarded). Hostile-marker rejection (overlap invariant). Bounded scans (64/8/20k). Python byte-parity pinned. Env-test hygiene (no process mutation after observed flake — contrasts cold_prefix TTL test which mutates DISABLE_* unguarded; safe there: single writer, no other readers). FINDING-011 EVIDENCE+: FOURTH truthiness convention here ({0,false,no,off} falsy, default-ON, ""=ON) vs protect_reads (""=OFF) vs cold ({1,true,yes}) vs env_truthy (+on). `off`/`""` mean opposite things across flags. No other bugs.

Next: transforms/kompress (913) + kompress_remote (1184).

## Entry 118 — kompress.rs (913 complete, ml-gated)
OK: ModernBERT token-compressor (350-word chunks, 512 trunc, max-score-per-word, >0.5 threshold, top-ratio API, must-keep net, 10/64-word floors, joined-spaces output, CCR owned by dispatcher with canonical <<ccr>> marker). Load paths files/pretrained/cache-only (261MB-download-safe defer + WSL-symlink diagnostics + NPU static512/MatMulNBits candidate ladder + per-candidate loud warn). ORT preload gate (third path after magika/fastembed). Mutex serialized inference (CPU no-batch-parallel parity). Static-seq right-pad masked (parity-identical). Stable top-k double-sort (CPython parity). Must-keep documented look-around→\b imprecision (version-fragment over-keep, safe direction). Env threads best-effort warn-continue. Tests config/parity-less unit (model tests need weights). FINDING-011 EVIDENCE+: MUST_KEEP exact-"0" only (untrimmed) — fifth convention. Env-mutating tests unguarded (minimal parallel risk here; noted). No prod bugs.

Next: transforms/kompress_remote (1184) + live_zone (4964, chunked).

## Entry 119 — kompress_remote.rs (1184 complete)
OK: no-ml drop-in (same registry name; CCR proxy-local, endpoint stateless; transport injected — core has no HTTP client; fail-open everywhere = Python blanket-except). Marker-pays gate whole-payload strict-smaller in one unit (7bd4dbaf: old 0.8 word-ratio shipped unmarked-unrecoverable + admitted unpayable markers; store-before-gate orphans TTL-bounded). Coercions document divergences (negative→fail-open vs Python carries; compact-vs-repr model_used — garbage field either way). Header case-insensitive replace (documented vs Python dict.update). Path override (fail-open-invisible 404 lesson → url() loggable). Tests ~35. MINOR-023: gate-rejected UNCHANGED content returns Compressed(None) → registry flag compressed:true with identical content (conflated with transformed-unmarked case); callers must check tokens/content, not flag alone. Low. No other bugs.

Next: transforms/live_zone (4964) — read in ~300-line chunks.

## Entry 120 — live_zone.rs (4964 complete, 17 chunks)
OK (core dispatcher): floor/ceiling live-zone model, byte-range surgery (RawValue pointer arithmetic; prefix/suffix literally copied; UTF-8 + RawValue bailouts; no round-trip verify by cost, pinned by byte_fidelity test), per-type 512B thresholds, dispatch memo (process-global pure-fn cache; key text+type+ratio+kompress-flag; skip-tier for NoOp; errors never cached; acceptance recomputed — threshold-change safe), bytes-gate before token-gate (measured 99.6% justification), Kompress non-blocking (get not get_or_init; warm off-path; NPU-stall lesson), CCR hash-before-gate but put-after-admit (no orphans/markers — strictest of the family), tool guards (CCR unconditional+undecaying > verbatim > byte-exact > protected-read(content-settled) > lossless-only; third-party headroom_retrieve collision accepted+documented), exclusions (hot-zone types, ctx-digest double-pointer lesson, frozen floor), Bedrock Converse typeless-text routing, OpenAI chat (latest tool+user separately) + Responses (same-frame outputs) dispatchers, read-protection both wire shapes, byte-exact builtins on OpenAI path, message-part first-text-only, manifest/tokenizer discipline. Tests ~70 incl. guard matrix, frozen clamp, memo properties, size-gate path, byte-fidelity refs.
FINDINGS (all cross-provider consistency, Phase-C incompleteness class):
- INCONSISTENCY-006: all-messages StringContent passes None CCR (line ~1151) vs live-zone passes store — user-prose blocks never marked in all-messages mode.
- INCONSISTENCY-007: OpenAI chat (3515) + Responses (4190) pass None CCR + DispatchConfig::default() — operator --exclude-tools ignored + no markers on both OpenAI paths; Anthropic honors both.
- INCONSISTENCY-008 (pending content_router pass): Anthropic uses detect_content_native, OpenAI x2 use detect_content_type (retired oracle) — verify what native is.
- FINDING-012: Responses `latest_message` hardcoded None (line 4045) — message items never enter live zone though docs (3914-3917) list latest message; manifest index always None. Doc-code mismatch (conservative direction).
- INCONSISTENCY-009: Responses retrieve-name match (exact + __ suffix) misses `mcp_` single-underscore alias the Anthropic planner covers (3 spellings pinned) — narrow unredeemable-marker hole on Responses path.
- Note: live_zone does NOT use base::split_frozen (own floor logic, correctly clamped) — FINDING-010 stays dead-code-only.
No single-provider logic bugs.

Next: transforms/content_router (4532, chunked) — resolves INCONSISTENCY-008 + CHECK-005/007.

## Entry 121 — content_router.rs (4532: code 1-2554 fully read, 160 tests surveyed + key ones read)
OK: strategy/config enums, 4 savings profiles, ToolSignature (5-item merge, null-tolerant typing, word-boundary patterns), ContentRouterConfig defaults pinned by test, bash parsing (wrappers/env/numeric-args, git-grep, bash -c recursion), mixed/json-shape heuristics, tool_call arg/command extraction (Anthropic dict + OpenAI string + Codex list), envelope strip (full-string only, numeric returncode, never-empty-probe, tested), JSON block extractor (string/escape aware), section splitter, net-cost TTL + image-flat-cost (57x-146x lesson) + netcost counters, native detection = strip + oracle + HTML-misroute guard, strategy map, STAGE-0 lossless-first (primary-then-others, diff-fold quarantined to diff-looking content — unrecoverable-line lesson), lossless-only stop, lossy-after-fold (5% extra + Diff/looks_like_diff double-exclusion), external-compressor contract (MIME/wildcard match, empty/expansion fail-open, own-estimator counting, recoverable persist best-effort), SmartCrusher→Kompress→Log fallback chain (chain documents attempts), per-compressor enable gates, Kompress size gate (chars>tokens*4 strict-gt, 0 disables, TextCrusher off-ramp + chain label, parity-tested), segment (blank-delimited + continuation-aware windows, lossless join), Otsu/adaptive threshold (degenerate→floor, saturating casts NaN-safe), relevance split (all fail-open keep-all), two-tier CompressionCache + frozen verdicts (FIFO-4096, #1307 recoverability rule, clear() drops both — register_on_clear parity), 160 tests incl. datetime regression (2026-08-23 incident), envelope pin, gate boundaries, image-cost.
INCONSISTENCY-008 RESOLVED→CONFIRMED (narrowed): native = strip + oracle + HTML guard; OpenAI x2 call oracle directly so miss (1) envelope strip — pinned native-only by test, and OpenAI paths compress exactly enveloped tool outputs; (2) HTML-misroute guard. Datetime fix lives IN oracle (content_detector) so OpenAI paths have it.
FINDING-013: ToolSignature::from_items empty-input mints wall-clock-nanos hash (213-220) — contradicts "deterministic and persists to disk" contract (1293); empty-output patterns never match + distinct persistent signature per call (low; fail-safe direction).
MINOR-024: bash_program wrapper-arg skipper only skips flags/numerics — `timeout -s KILL 10 grep` returns prog "kill" (safe: fail-open, no fold).
MINOR-025: matches_pattern misses plurals ("errors"/"messages"/"warnings" match nothing) — TOIN recall gap, conservative.
MINOR-026: from_json(array) yields field_count 0 vs from_items proper count — constructor inconsistency (from_json likely legacy).
MINOR-027: try_kompress falls to from_pretrained (model DOWNLOAD) on sync path — latent only (apply_strategy stated off-proxy-path; live_zone uses from_cache-only).
MINOR-028: CompressionCache results/skip maps have no cap/sweeper; expired-but-unrevisited entries linger past TTL (bounded by block cardinality; check Python parity).
MINOR-029: cache locks use .lock().unwrap() — poison (panic while held) breaks all future cache ops; cf. magika.rs deliberate Poisoned handling.
CHECK-013 (new): plan_relevance_split `segs.len() > max_records` → keep-all; verify vs Python (skip vs truncate when over cap).
CHECK-014 (new): resolve_detect_backend "python" arm — verify any consumer honors it or vestigial.
CHECK-005 stays (live_zone splice sites, not this file). CHECK-007 stays (magika-config vs oracle-config reconciliation not in this file — no normalization in native/strategy map; needs proxy pass).

Next: transforms/code_compressor (3242, chunked).

## Entry 122 — code_compressor.rs (3242: code 1-2494 fully read, tests surveyed + parity pins read)
OK: grammar-version parity docs (pins + canary + ASCII-scope honesty), 13-language LangConfig tables (C# container/opaque, PHP namespace hoist, Bash all-opaque backstop), coerce (aliases → Unknown→content-detect; documents Python ValueError-swallow fix), py_round_int/py_round3 hand-verified + test-pinned (incl. -2.5→-2), prefilter→tree-sitter(fewest-errors, stable tie-break, TS/C++ superset zeroing)→regex-fallback detection, syntax breaker (window-20/min-10/cooldown-30m, per-lang isolation, poison-tolerant locks, env kill-switch, /stats surfacing), never-serve-broken-code (whole-output verify + Python-only per-candidate recovery retry gated on valid source + ratio<0.05 guard), Bash validated-byte-for-byte early return, export/decorator/doc-comment attachment, Allman/C++-semi edge handling, CJK-aware symbol relevance, body-budget allocation, statement-based truncation (first-stmt always kept), byte-identical-to-Python pins (C#, PHP), breaker/bash/CJK/coerce test suites. No bugs.
MINOR-030: decorated_definition with class child but zero decorator children would land in function_signatures (line 2012) — unreachable per grammar (decorated_definition always has ≥1 decorator); re-parse still guards output.
Note: router recomputes CodeAware tokens via split_whitespace, ignoring result's chars/4 fields — consistent within router, no double-count.

Next: transforms/diff_compressor (1781, chunked).

## Entry 123 — diff_compressor.rs (1781: code 1-1225 fully read, tests surveyed + parity pins read)
OK: 20-fixture parity contract, file cap (heaviest-first + dropped names in stats), hunk cap (first+last+top-scored middle, line-number resort), context trim (change±N + backslash-marker always-keep + no-change-hunk head rule), CCR (MD5[:24] verified vs known vector, pre-marker count captured — parity-by-1 documented, store-or-warn persistence fixing prior dangling-marker), split('\n') parity pin, pre-diff preservation, rename/combined-diff/no-newline bugfixes (each test-pinned), CJK bigram scoring, mode/binary parity-loss surfaced via stats (100644 hardcode, `Binary files differ` hardcode — honest observability), pass-throughs, OTel span. No logic bugs in covered paths.
FINDING-014: `parse_warnings` is ALWAYS empty — `let warnings` (line 690) never `mut`, zero push sites; the documented >4-parent octopus-merge warning (600-604) doesn't exist. Dead observability field (low; input vanishingly rare, falls into "other"-line branch).
CHECK-015 (new): quoted-path headers (`diff --git "a/sp ace" "b/sp ace"` — git's DEFAULT quoting for spaces/non-ASCII) match neither diff_git_regex nor old/new_file regexes → file section misparsed (pre-diff blob or prior file's hunk). Verify Python: same regexes = parity-bound real-world gap; quote-aware = parity BUG here.
MINOR-031: score_hunks word gate uses byte `len() > 2` vs Python char `len()` — 2-char multibyte query words boost in Rust only. Negligible.
Note: removed lines starting with `--` (`---foo`) excluded from deletion counts at parse (800) but treated as change at trim (1015) — count-only, parity-bound, output bytes unaffected.

Next: transforms/log_compressor (1803, chunked).

## Entry 124 — log_compressor.rs (1803: code 1-1399 fully read, tests surveyed + key pins read)
OK: 6-format AhoCorasick detector (first-100-lines, one-hit-per-line parity comment), word-boundary level classifier (LeftmostLongest so "warning" beats "warn" + boundary recheck — subtle and correct), 7-flavor trace state machine (per-flavor terminators, chained-exception blank-line fix, cap-hit continuation so collapse-not-alignment decides, opener re-check on terminate line), frame collapse (head+app budgets, chain-head immunity, dropped-run markers score-0.8 to survive global cap, dropped indices excluded from context pass — undo-guard), summary detector, first/last error endpoints + score fill, conservative dedupe (prefix-preserving, timestamp-in-prefix weakens timestamped-log dedupe — conservative/test-pinned direction), global adaptive cap (score-desc truncate, line-order restore), BTreeSet-by-line_number selection (Ord/Eq/Hash consistent), CCR 0.5 threshold + loud store failures, score cap 1.0 pinned, summary format pinned. No bugs.
Note: `select_with_first_last` reuses keep_first/last_error flags for FAIL lines (naming only); `let _ = ();` leftover at 1113 (cosmetic).

Next: transforms/search_compressor (1478) + content_detector (1480), chunked.

## Entry 125 — search_compressor.rs (1478: code 1-953 fully read, tests surveyed)
OK: raw-verbatim rendering (zero-pad timestamp incident documented + test-pinned — mis-parse now ranking-only, never data corruption), 3-tier parser (colon/dash/permissive with whitespace guard, adjacent-separator rejection, Windows drive skip, extension-aware dash walk with documented Makefile-vs-CVE tradeoff, all byte-index-safe on ASCII separators), scoring (char-count word gate — correct here, error/warn/importance mapping, keyword boost, 1.0 cap), selection (score-order files, per-file first/last + score fill, O(n log n) dedup, line-order restore, adaptive budget on emitted raw text), grouped heading layout, CCR thresholds + loud store failures, ~30 parser regression tests (Windows/dash/date-stamp/zero-pad). No bugs. (Checked: no stats double-count — global-cap `continue` skips per-file accounting.)
MINOR-032: parse stores the TRIMMED line as `raw`, so leading indentation of grep output is lost in rendering (parse-necessary trim; affects only indented inputs).

Next: transforms/content_detector (1480, chunked).

## Entry 126 — content_detector.rs (1480 fully read incl. tests)
OK: dispatch order + gates pinned, JSON (incl. concatenated-objects #1741 normalization), diff/log/search/code detectors with false-positive hardening (search floor-2 + ratio + markup/`=` guards from 2026-08-23 incident; datetime fix lives HERE so OpenAI paths inherit it), HTML sampler char-boundary-safe, tabular (md + delimited with prose guards), config (TOML-parse/INI-accept disambiguation, front-matter + prose guards, Python-parity confidence pins to 1e-9), per-line regex application (no (?m) needed — verified not a bug), Python max()/dict-order tie-break replicated with Vec+find. Tests pin tags, thresholds, incident regressions.
DRIFT-008 (new, documented in-test at 1457-1460): single JSON object → Rust PlainText vs Python json_array(is_object) — real routing divergence (Kompress vs SmartCrusher), pre-existing, explicitly deferred.
MINOR-033: try_detect_delimited most-common-count via HashMap max_by_key — tie outcome nondeterministic run-to-run (metadata ncols/delimiter only; routing-equivalent today).
MINOR-034: SEARCH_RESULT_PATTERN can't match Windows drive-letter paths (`C:\…:42:`) — absolute-path Windows grep output never routes to Search though search_compressor's parser handles drives (detector/router gap).
CHECK-016 (new): plain `diff -u` headers (`--- foo` without `a/` prefix, count-less `@@ -1 +1 @@`) match no DIFF_HEADER_PATTERN arm — verify Python parity (same = parity-bound gap for non-git diffs; broader = parity bug).

Next: transforms/read_lifecycle (1416) + read_maturation (941) + read_protection (part), chunked.

## Entry 127 — read_lifecycle.rs (1416: code 1-647 fully read, tests surveyed)
OK: fresh/stale/superseded model, message-granularity causality (correct for cross-message read→edit), frozen-prefix demotion (cache-safe), range-aware supersede (full-file covers all; partial needs containment; 2000 default), OpenAI-chat + Anthropic-block replacement, min-size gate, SHA256-24 CCR + best-effort store + loud failure, load-bearing "Retrieve original: hash=" phrase documented, classify-without-rewrite split for retrieval path. Tests cover both formats + chains + gates.
MINOR-035: same-message read→edit (same msg_index) never stale — message-granularity blind spot; fail-open (keeps content), ambiguous under parallel-call semantics.
CHECK-017 (new): Responses-API shape (`function_call_output` items in `input`) matched by neither role=tool nor tool_result-block logic — verify lifecycle isn't silently no-op on Responses path.
CHECK-018 (new): tool-name allowlists are Claude-harness-shaped (Read/Edit/MultiEdit/NotebookEdit/Write) — shell/apply_patch/function-call edits don't stale-mark; verify proxy normalizes names or accept harness-scoped behavior.

Next: transforms/read_maturation (941, chunked).

## Entry 128 — read_maturation.rs (941: code 1-491 fully read, tests read to 680 + surveyed rest)
OK: hold-while-active/mature-when-quiet model (turn-granularity activity scan, per-file last-touch incl. reads as touches, max-hold safety valve), deterministic replay via per-session matured map (re-marks original content if proxy re-feeds it; file re-activity handled by fresh re-read under new ID), lifecycle-marker immunity (no double-compress), frozen-prefix skip, min-size gate, CCR + loud failure, load-bearing phrase, breakpoint relocation (strip held region + re-anchor whole marker preserving ttl — documents the bare-ephemeral downgrade trap). Timing boundaries test-pinned (4 holds / 5 matures / other-file no-reset / max-hold cap). No bugs.
MINOR-036: relocate_cache_breakpoint with earliest holding index 0 strips breakpoints with nowhere to re-anchor (loop 0..0 empty) — pathological (tool result as first message); also mixed-ttl regions re-anchor with the LAST marker seen. Edge-only.
Same CHECK-017/018 exposure as lifecycle (OpenAI-chat + Anthropic-block shapes only; same tool-name allowlists).

Next: transforms/read_protection (?) — locate + read, then tag_protector (1272).

## Entry 129 — read_protection.rs (362 fully read)
OK: two-gate model (command-is-read via structural bash_program parse + cd-peel + wrapper/nested-shell see-through; content-not-data via oracle with protect-by-default), write exclusion (redirect/tee/heredoc/bare-sed), lockfile carve-out by name (with Cargo.toml control), wire-shape command extraction (Anthropic obj / OpenAI JSON-string / Codex token-list / native string), config-stays-protected, empty-releases, off-by-default flag with cross-provider single consult. Tests pin each gate incl. adversarial shapes. No bugs.
MINOR-037: second `tool_call_command_text` in this file diverges from content_router.rs's same-named fn on string input (router drops non-JSON strings, this one passes them through) — correct per use-site, trap for future callers; consider one fn with a mode flag.

Next: transforms/tag_protector (1272, chunked).

## Entry 130 — tag_protector.rs (1272: code 1-744 fully read, tests surveyed)
OK: single-pass byte walker (memchr skip, proptest-found OOB guard, quote-aware attr lexer), HTML5 allowlist (case-insensitive, lazy-lowercase), block vs marker-only modes, stack matching with mid-stack close (truncate+retain collapse), orphan open/close verbatim (never repair), salted placeholder prefix on collision, offset-splice emit with overlap loud-bail, restore with Hotfix-A9 discard-wrap (documented 350-req incident, ERROR-level structured log, symmetry/no-injection/idempotence invariants). All 5 Python bugfixes + hotfix test-pinned. No correctness bugs.
MINOR-038: self-closing flag set by ANY bare `/` outside quotes (`<custom a=b/c>` misflags self-closing → body unprotected) and cleared by later whitespace (`<tag / >` misflags as open → verbatim fallback). Marginal shapes, protection-lost direction for the former.
MINOR-039: 16-salt-exhausted fallback is a FIXED constant, not a UUID — comment overclaims; double-collision (adversarial) input restores ambiguously.
MINOR-040: `spans.retain` per matched close makes sibling-heavy inputs O(blocks²), not linear as documented — bounded by tag count (hot path 0-10), Python-bug class fixed in practice.

Next: transforms/lossless_compaction (1050) + compression_units (1064), chunked.

## Entry 132 — lossless_compaction.rs (1050: code 1-691 fully read) + compression_units.rs (1064: code 1-501 fully read)
OK lossless: self-verifying folds (round-trip-or-revert + net-shrink gate on every kind), timestamp-row exclusion (documents WHY inverse-check can't catch it — semantic loss that round-trips), hand-written-marker distrust (non-overlap invariant enforced on unfold), trailing-newline-preserving split/join, search file-vs-dir two-fold best-pick, diff_index quarantine (caller-side, documented), per-call env kill-switch (default-ON, falsy {0,false,no,off} — FINDING-011 family variant, documented as mirror of Python). Roundtrip tests incl. timestamp guard. No bugs.
OK units: guard lattice (immutable > user/system/assistant opt-in > cache_zone > byte floor), marker-preserving segmentation (markers byte-passthrough, segments re-gated — MINOR-015 already recorded), tokenizer-gated acceptance (not word estimates), lossy-unmarked shell guard AFTER size gate (correct order; original-marker path can't reach it), slot-opaque splice model, reason taxonomy. No bugs.
CHECK-005 UPDATE: rejected-carries-larger-replacement shape CONFIRMED at compression_units.rs:357-372 + 419-436 (`.compressed` = LARGER text, `modified` = false). Splice-site honor check needs proxy adapters pass — still pending.

Next: transforms/compression_batches (1270) + compression_summary (607), chunked.

## Entry 133 — base/safety/recommendations/detection + upgrades (all fully read)
FINDING-010 UPGRADED (confirmed-in-code, dead-in-practice): base.rs:13-16 — when frozen_message_count >= messages.len (entire prefix cached), split_frozen returns frozen=&[] EMPTY and everything as mutable. The inversion is real and PINNED by test split_frozen_all (91-93 asserts frozen empty). Zero callers repo-wide (verified rg) + trait unwired — staged only. If ever wired, the all-cached case busts the whole cache. Test pins the bug too.
INCONSISTENCY-010 (new): detection.rs locked-design doc says regex oracle "does not run in production detection" (retired, magika+unidiff only) — but detection::detect has ZERO callers outside its tests (verified rg incl. proxy), while the live proxy path (live_zone) runs the oracle per block via detect_content_native/detect_content_type. Doc describes an unwired future; hot path runs the "retired" oracle.
FINDING-015 (new): observability.rs MetricsObserver::record_compression is a NO-OP (&self, no interior mutability — counters permanently zero) and TestObserver likewise a placeholder; verify in proxy pass whether /stats reads these fields (live always-zero bug) or they're dead code.
MINOR-041: compress_batch_with_router indexes batch.entries[0] (line 435) with no empty guard — pub fn, panics on empty batch (constructors never produce one).
MINOR-042: compression_summary category/key ordering nondeterministic (HashMap iteration + unstable sort_by count-desc at lines 111, 304) — equal-count category order varies run-to-run; cache-bust risk IF summaries land in cached output (verify consumer in smart_crusher/proxy pass).
OK safety.rs: pair lattice both shapes, orphan-drop, multi-result dedup. OK recommendations.rs: startup-once TOIN loader, loud failures, shared AuthMode (drift-merge documented), PR-F3 wiring pending (documented dead surface).
CHECK-005 still pending (proxy splice sites).

Next: transforms/cache_aligner (603) + relevance_split (388) + unidiff_detector (322), chunked.

## Entry 134 — cache_aligner + relevance_split + unidiff_detector (all fully read)
OK cache_aligner: detector-only (deep-copy return), UUID/ISO/JWT/hex classifiers, sample slicing safe-by-construction (classified tokens ASCII-only — verified), BTreeMap warning counts (deterministic), prefix-hash change tracking via caller-held state. MINOR-045: string-only system content — Anthropic block-list system content invisible to detection/score/metrics.
OK relevance_split: lossless keepends segmentation (tests pin), query composer, Otsu+floor, over-cap keep-all TEST-PINNED → CHECK-013 RESOLVED (intentional fail-open, pinned by split_respects_max_records_cap). MINOR-044: otsu_threshold uses partial_cmp().unwrap() — NaN score panics (content_router twin uses unwrap_or(Equal)); reachable via pub adaptive_threshold on NaN input. CHECK-019 (new): segment/Otsu/adaptive/plan duplicated between content_router.rs and relevance_split.rs (different signatures, already-diverged join semantics) — verify which callers use which.
OK unidiff: prefilter + catch_unwind around unidiff 0.4.0's known `+++`-without-`---` panic (version+line documented), empty-patch ≠ diff, xtrace/canary tests. Combined-@@@ gap deliberately punted + documented (note: diff_compressor handles @@@ if routed via oracle path).

Next: transforms/thinking_compactor (757) + compressor_registry (450) + config_compressor (415), chunked.

## Entry 135 — thinking_compactor + compressor_registry + config_compressor (all fully read)
OK thinking_compactor: live-verified billing model (signature-pin re-expansion → convert-don't-edit; determinism-memo for cache stability with SHA-256 key documented as internal-only), conservative bills_prior_thinking (version parse with saturating date-segment handling; Opus-4.5 exclusion documented), shrink-or-keep acceptance (word_count filter), cache_control preservation across conversion, keep-last-turns windowing, Kimi/GLM/R1 shapes (drop vs kompact modes), unterminated-span passthrough, fail-open throughout, Python-reference test values. No bugs.
OK registry: explicit-registration model (entry-point discovery impossibility documented as capability difference, not oversight), opt-in selection with "*" + missing-name warn, BTreeMap sorted order, unwired-on-purpose rationale. No bugs.
OK config_compressor: store-FIRST elision (no marker without stored original, test-pinned), block-scalar/multiline data guards (over-broad by design), INI continuation-line care, schema-fold size competition + faithful-rendering gate, min-savings gate. No bugs.
MINOR-046: ConfigCompressionResult::compression_ratio returns 0.0 on empty original vs 1.0 convention elsewhere (likely unused; check on proxy pass if surfaced).

Next: transforms/anchor_selector (1189) + magika_detector (736), chunked.

## Entry 136 — anchor_selector + magika_detector (both code fully read)
OK anchor_selector: Python json.dumps parity writer (separators, sort_keys, ensure_ascii incl. surrogate pairs + 0x7F handling — verified correct), budget/strategy/weight logic with documented Python mirrors (truncation, max(1,...) slots, actual-count middle region), density scoring with all-divisions-guarded, BTreeSet determinism, stable-sort tie-break. MINOR-047: f64 exponent formatting (Python '1e+16' vs serde '1e16') diverges item hashes on extreme floats — fixture-rare. Note: length-score uses compact vs Python-spaced JSON lengths (normalization absorbs; fixture-pinned).
OK magika_detector: explicit label arms (no group confusion), unmapped→PlainText, singleton Mutex<Result> with Poisoned/Init/Inference loud errors, empty shortcut, degrade-gracefully tests both arms. CHECK-007 ANSWERED: magika yaml/toml/ini → SourceCode CONFIRMED in map (line 473) vs oracle StructuredConfig — real divergence, live-MOOT while detection::detect unwired (INCONSISTENCY-010), PR5-owned per code comments. No live impact today.
No bugs in either file.

Next: smart_crusher/ + text_crusher/ directories, then pipeline/, then proxy (69 files).

## Entry 137 — smart_crusher core (crusher/crushers/planning/orchestration/hashing/stats_math) + text_crusher
OK crusher.rs (1450 lines read): entry lattice (parse→recurse→python-safe serialize), concatenated-JSON normalization, audit-safe scan-before/splice-after with multiplicity + fail-closed option, depth cap, per-type dispatch, CCR sentinel shape rationale, prose-source hash discipline (#2694: hash the PRE-processing array — verified correct), lossless-first + lossless-only debug_assert invariant, marker-gated lossy, canonical-bytes-reuse for hash+store. CHECK-020 (new): crush_mixed_array dict-group DISCARDS crush_array's ccr_hash/dropped_summary (line 1232 `..`) — group row-drops go markerless; verify Python parity (same = parity-bound contract hole; propagates = silent-drop bug here). MINOR-048: scan_protected_rows parses raw content, missing the concatenated normalization two lines later (audit-safe blind to `{...} {...}` inputs).
OK crushers.rs: BUG#1 already FIXED via percentile_linear (numpy-linear verified) BUT module doc (18-31) still claims ported-as-is pending commit 7 — MINOR-050 stale doc. BUG#4 clamp verified + test-pinned. round_ties_even parity primitive correct. format_number_repr int/float approximation sound (matches Python int rendering via i64 branch). crush_object recomputing-cap mirrors Python inefficiency deliberately.
OK planning: 4 planners mirror-documented; top_n unbounded-additive (no prioritize clamp) — CHECK-021 (new): verify _plan_top_n same in Python. Stable sorts, NaN-guarded anomaly loops, TOIN stubs documented.
OK orchestration: dedup lowest-wins, interleaved stride fill, critical-first over-budget (documented may-exceed), value-hash for scalars (Null→"None" Python-str parity). Tests pin guarantees.
OK hashing (SHA-256[:8] misread-once-now-pinned) + stats_math (sample denominators, format_g %.4g incl. exp-boundary + mantissa trim — fiducial, fixtures lock).
OK text_crusher (code fully read): extractive, deterministic order, zero-keep passthrough, char-budget max(1), ASCII byte-identical dispatch, ICU CJK path with width-fold (output stays verbatim), textbook BM25 deliberately NOT shared-scorer (documented why), salience rules byte-safe. No bugs.
No live-path bugs in this batch.

Next: smart_crusher/compaction/* + analyzer/classifier/anchors/constraints/outliers/statistics + pipeline/offloads + reformats (survey + targeted), then proxy.

## Entry 138 — compaction walker/compactor + analyzer select/estimate + orchestration/pipeline core
OK walker (fully read): recurse-before-compact cascade, store-required compact_with_store (#2694 dangling-marker lesson applied), stringified-JSON re-emit without double-encode, opaque marker+store with loud failure, hash-identical-regardless-of-store (contract stability). Depth relies on serde's 128 (no own cap; crusher path caps at 50) — safe in practice.
OK compactor (core read): core-ratio/heterogeneous/discriminator routing, freq-desc+alpha column order (deterministic), nested flatten bounded, stringified-JSON recursion (≥2 objects), opaque store-write with warn, store-identical IR. Tests pin discriminator/table/shape behavior.
OK analyzer select_strategy/estimate (read): crushability gate, time_series/logs/search routing, ZeroDivision mirror (0.0 + unreachable guard). MINOR-051: message-field pick uses BTreeMap alphabetical order vs Python insertion order — diverges only with 2+ message-like fields straddling unique_ratio 0.5 (comment acknowledges determinism≠identity).
OK pipeline traits+orchestrator (core read): fail-open error taxonomy, type-enforced cache_key, parallel bloat estimation, fallback-offload gate, zero-byte discard, saturating totals, must-not-panic contract. Test-only unwraps confirmed (822 in-test).
No live-path bugs.

Next: pipeline/offloads/* + reformats/* + config + text-adjacent (spreadsheet/tabular_ingest, html_extractor, kompress*) survey, then proxy.

## Entry 139 — offloads/prose_field (+siblings) + kompress + html_extractor + tabular_ingest
INCONSISTENCY-011 (new): prose_field.rs:90 ignores CcrStore::put's bool — the ONLY store-write call site without loud-failure handling (all others `if !put → warn!`, incl. json/diff_noise offloads; log/search delegate to compress_with_store which warns). Failed write → marker dangles silently. Breaks the project's own invariant.
FINDING-016 (new): kompress.rs compress_inner loops `chunk_start += config.chunk_words` with no validation — chunk_words=0 (pub field, config-supplied) hangs the request thread forever. Needs `.max(1)` clamp (min_words already has MIN_WORDS clamp — asymmetric). from_cache/from_pretrained discipline exemplary (cache-only hot path, loud defer diagnostics incl. WSL-symlink lesson).
OK html_extractor: fail-open (empty→zeroed, garbage→"", bad URL retried without URL), metadata-before-parse ordering, char (not byte) lengths. No bugs.
MINOR-052: tabular_ingest::parse_csv is quote-unaware (naive delimiter split; quoted commas/newlines mis-split). Best-effort ingest; bounded to tabular routing.
Remaining core odds (surveyed via hazard-grep, no production unwraps outside tests): smart_crusher small modules (classifier/anchors/builder/config/constraints/error_keywords/field_detect/observer/outliers/statistics/traits/types/ir/compaction-classifier), pipeline config/mod/reformats, kompress_remote, spreadsheet_ingest, transforms/mod.rs — spot-check on proxy pass if referenced.

Next: PROXY (69 files) — adapters/splice sites (CHECK-005), exclude-tools wiring (INCONSISTENCY-007), Responses coverage (CHECK-017), MetricsObserver consumers (FINDING-015), MINOR-042 consumers.

## Entry 140 — proxy adapters: outcome enum + exclude-tools end-to-end
CHECK-005 RESOLVED: proxy never calls compress_unit_with_router (zero callers repo-wide — unit/batch path unwired like registry). Live path uses LiveZoneOutcome enum (Modified carries new bytes / NoChange carries none) and all three adapters match it correctly (anthropic 400/457, chat 137/165, responses 148). The rejected-carries-larger shape exists only in the unwired unit path. No live splice bug.
INCONSISTENCY-007 UPGRADED → FINDING-017 (operator-facing): --exclude-tools honored end-to-end on Anthropic only (config → proxy.rs:5375/batch/routed/bedrock → adapter → DispatchConfig). OpenAI chat adapter (live_zone_openai.rs:57) and Responses adapter (live_zone_responses.rs:59) take NO exclude_tools param and call the 3-arg core fns that hardcode None CCR + default config. Flag help text is provider-agnostic; defaults protect file reads/search. Codex/Responses + OpenAI-chat operators silently get zero exclusion.
FINDING-018 (new): --protect-tool-results parsed (config.rs:1697/2443/2736) but never read anywhere — dead flag on ALL providers. Operator sets it, nothing happens.
No bugs in adapter outcome plumbing.

Next: proxy request flow — lifecycle/maturation/dedup/cold-prefix wiring per provider (CHECK-017/018), MetricsObserver consumers (FINDING-015), summary consumers (MINOR-042).

## Entry 141 — proxy flow: dedup/lifecycle/maturation wiring + dead flags
FINDING-015 RESOLVED (dead, not live-zero): MetricsObserver has zero consumers repo-wide (only its own tests + a doc-mention in smart_crusher/observer.rs). Misleading dead code; no /stats reads it.
FINDING-019 (new, big): read-lifecycle + read-maturation subsystems UNWIRED — flags parsed (config.rs:1701/1705) but `.read_lifecycle`/`.read_maturation` never read anywhere (verified rg); ReadLifecycleManager.apply, ReadMaturationManager.apply, relocate_cache_breakpoint have ZERO proxy callers. Only live use is classify() for retrieval-time stale warnings (ctx_offload.rs:987). CHECK-017 + CHECK-018 SUPERSEDED by this: not a Responses-shape gap — nothing runs on any provider.
CHECK-017 PARTIALLY LIVE elsewhere: apply_cross_turn_dedup explicitly no-ops on Responses `input` shape (cross_turn.rs: "nothing this pass knows how to dedup") + skips streaming chat (pointer-unresolvable, documented). So cross-turn dedup = Anthropic-messages (+non-streaming chat) only, default-off anyway.
OK chat-path stream guard: documented rationale (no retrieval tool on streaming path → pointers read as deleted content → retry loops). Good incident-driven reasoning.
No request-flow bugs in verified sections.

Next: proxy Responses branch + routed/bedrock exclude paths + handlers/sse/websocket/memory/ctx/observability/cache_stabilization survey.

## Entry 142 — proxy: codex lift/restore + prior_thinking + adapter parity
OK responses lift/restore: carrier split with kept_index+shift reinsertion, idempotent restore, empty/changed-count fallbacks (first-carrier coalescing), lift-gated on carrier presence (env lookup off hot path), serialize-failure fallbacks both directions. Index stability holds (Responses dispatcher rewrites in place, never reorders). No bugs.
OK prior_thinking: probe-backed (2026-09-02 200s, 2026-09-11 tail-divergence regression with gate fix), last-assistant intact, never-empty-message, idempotent, pure-of-originals. No bugs.
OK three adapters share identical outcome plumbing (Modified/NoChange/NotJson/NoMessages + normalization-stitch). OpenAI ones lack exclude_tools (FINDING-017) but are otherwise structurally sound.

Next: cache_stabilization/* (20 files) + ctx_offload core + memory/ctx/observability/sse/handlers survey.

## Entry 143 — cache_stabilization: E1/E2/E3/E4 + B3 roster pin
OK tool_def_normalize (fully read): stable sort, dual-shape name lookup (Anthropic + OpenAI-chat; Responses top-level name covered by first arm), MD5 fallback for unnamed (measured perf comments), exact already-sorted detection, marker gate, schema recursion preserving arrays, byte-stability tests + proptest. No bugs.
OK E3 auto-place (core read): customer-marker-wins, idempotent, malformed-safe, single last-tool slot, Applied{0} vs Skipped telemetry distinction. Caller-gated PAYG by contract. No bugs.
OK E4 cache key (core read): length-prefixed (model, system-hash, tools-hash) SHA-256, customer-key-wins, deterministic/idempotent, both OpenAI shapes with instructions-first extraction. No bugs.
MINOR-053: roster-pin first-sighting no-op detection (`out.len() == appended.len()`) fails with unnamed tools → reports changed though bytes identical → caller re-serializes (whitespace normalization → first-turn cache-bust + spurious log). Narrow (first turn + unnamed tools + non-compact input).
OK B3 pin logic otherwise: duplicate-name safe, marker-decline-with-remember, per-session LRU with poison-tolerant lock, new-tools-to-tail.

Next: prefix_replay + usage_observer (large — structural survey), ctx_offload core, memory/ctx/observability/sse/handlers.

## Entry 144 — prefix_replay (structural) + ctx_offload core decisions
OK prefix_replay (decision cores read): append-only guard on canonicalized content (transport-churn robust), explicit ReplaySkip taxonomy (no silent wrong-forwards), scaffolding-withdrawal alignment, system-digest adoption gate, multi-stream alternates (item-11/item-25 lessons), chain identity, privacy discipline (structural paths; one bounded 120-char escaped exception). Fail-decline posture throughout. Line-complete verification deferred (8.5k lines) — guards read, transforms skimmed.
OK ctx_offload core: live-tail vs frozen gating (PR-J4 rebuild boundary), prior-before-positional (history-edit flip guard), verbatim exclusions honored, window vs frozen-violation counting kept apart.
NUANCE to FINDING-017: on Anthropic path --exclude-tools deliberately does NOT gate CTX-3 offload (documented + measured -6.6% vs -11.4%: offload keeps digest+preview+retrieval, so the protection argument doesn't carry). Exclusion stops live-zone lossy rewrites only. Operator-visible behavior differs by mechanism — flag docs should say so.
No bugs in read sections.

Next: memory/ + ctx/ + observability/ + sse/ + remaining handlers/ + compression/mod + bedrock envelope/vertex/foundry.

## Entry 145 — memory tool adapter: Responses shape broken end-to-end
FINDING-020 (new, confirmed end-to-end): memory/tool_adapter.rs is Chat-Completions-shaped; Responses `function_call` items fail at all four steps: get_tool_name reads `.function.name` (Responses: top-level `name`) → ""; get_tool_id reads `.id` (Responses: `call_id`) → ""; get_tool_input reads `.function.arguments` (Responses: top-level `arguments`) → {}; format_tool_result emits Chat shape (role/tool_call_id) instead of `function_call_output`. Consequence: handler has_memory_tool_calls → false → handle_memory_tool_calls `continue`s → memory tools never execute on Responses/Codex path. The CCR-rounds loop in proxy.rs IS wired for openai_responses (items_field="input", lines ~12450/12680) — the loop runs, the adapter blinds it. Meanwhile proxy.rs pending_memory_call_names/memory_trace_lines read `.name`/`.id` DIRECTLY (bypassing the adapter) — partially working, inconsistent with the adapter path (id always "" there since Responses uses call_id → outcome lines never match).
Fix shape: branch Openai adapter fns on item `type == "function_call"` (top-level name/arguments/call_id; format as function_call_output).
No other memory-adapter bugs found in read sections.

Next: sse/ + handlers/chat_completions + ctx/ + observability/prometheus + compression/mod + remaining proxy files.

## Entry 146 — sse/responses machine + chat handler + endpoint routing
OK sse/openai_responses.rs (fully read): id-keyed state (P1-17 out-of-order lesson pinned by test), dotted+undotted event tolerance, dual index-field tolerance, done-without-deltas fill, delta-sum authoritative (observability-only state), unknown-event warn+preserve, byte-safe preview. No bugs.
OK chat handler: explicit-route rationale, forward_http reuse (no forwarder duplication), max_tokens→max_completion_tokens one-way rename with null/absent care. MINOR-054: when BOTH keys set with different values, max_tokens silently dropped (deliberate GPT-5/o-series precedence; changes older-model requests that accept both).
OK compression/mod routing: explicit 3-arm enum, model-id sanitize scoped to Anthropic with byte-equal no-op paths. No bugs.

Next: dead-flag sweep in config + compression_decision/failure/quarantine/feedback + background_compression + ccr_retrieve_repair + openai_buffered_ccr + output_shaper.

## Entry 147 — dead-flag sweep: 10 parsed-stored-never-consumed flags
Method: every `pub bool` in Args/Config + `.field` read-sites repo-wide (construction site only = dead).
DEAD (FINDING-018 extended — full list): read_lifecycle, read_maturation (→ FINDING-019 unwired subsystems), protect_tool_results, code_aware_enabled, force_kompress_all, lossless (!! — the entire lossless-only machinery unreachable from proxy config), disable_kompress, disable_kompress_fallback, disable_kompress_anthropic, disable_kompress_openai.
Live controls confirmed: enable_kompress → main.rs:168 set_kompress_enabled (runtime atomic; the ONLY kompress switch). Dead-pair trap: --disable-kompress defaults TRUE but does nothing; enabling Kompress requires --enable-kompress — an operator toggling the disable flag gets zero effect either way.
Spot-checked live: ctx_drop_prior_thinking, output_shaper_enabled, strip_system_cache_breakpoints, image_optimize, context_edit, hold_*, split_cache_ttl, compress_user/system_messages (CLI surface), enable_batch_api/bedrock_native, memory/ctx/retry/cost flags — all consumed.
Recommendation class: either wire or remove; at minimum the dead flags' help text should say so (silent no-op flags are operator traps).

Next: compression_decision/failure/quarantine/feedback + background_compression + ccr_retrieve_repair + openai_buffered_ccr + output_shaper.

## Entry 148 — quarantine/failure/background/repair/buffered-ccr
OK quarantine: deliberate non-port with reasoned runtime analysis (spawn_blocking vs fixed pool), revisit triggers, tripwire test + parity metric registration. Exemplary decision record. No bug.
OK failure matrix: pure fn, documented precedence, env parse (positive-int else default), tested all arms. No bug.
OK background_compressor: claim-before-send dedup, full-queue drop+count, JoinError counted, stats. MINOR-055: new() spawns immediately — panics outside a Tokio runtime (construction coupled to runtime context).
OK ccr_retrieve_repair: neutralize-not-drop (alternation-safe), text-preserving, must-check-neutralized contract, Anthropic-shape scoped. No bug (tail assumed tests).
OK buffered_ccr (core read): retrieve-tool-gated buffering with ChatGPT-OAuth carve-out (server-side transcript), unverified-JWT sniff explicitly routing-only, synthesized SSE sequence. No bugs.
No live-path bugs.

Next: compression_feedback + output_shaper + compression_decision + ctx/identity+inject + observability/prometheus + remaining sse/handlers/bedrock/vertex/foundry/gemini/interceptors.

## Entry 149 — feedback loop + ctx/identity + hazard sweep (bedrock/vertex/foundry/interceptors/gemini/audit/net_offload/decision)
OK feedback: record-only loop (record_compression wired at proxy.rs:5621; best_strategy/get_compression_hints unconsumed — consistent with PR-B5 retiring request-time hints, no dispatch non-determinism). best_strategy HashMap-tie nondeterminism moot (unconsumed). Bounded maps (50/100/50/100). No bug.
OK ctx/identity (core read): length-prefixed SHA-256, churn-line exclusion with measured receipts (30→1 key changes, 1.6M→34k tokens), cache_control-blind first-message hash, derivation-change cost documented. High discipline. No bug.
OK hazard sweep: production code clean — all unwrap/expect hits in test fns except justified static-path expects (foundry) and HeaderValue::from_str on internal-ASCII strings (gemini; numbers/strategy tags — panic requires non-visible-ASCII in internal tags, practically impossible).
No bugs in this batch.

Next: bedrock invoke/envelope/eventstream + vertex + gemini handler core + interceptors core + ctx inject/extract + memory handler/backend + observability prometheus + sse rest + openai req/resp/stream + codex/ + compression ctx_offload rest.

## Entry 150 — bedrock envelope + interceptors framework status
OK bedrock/envelope.rs (core read): first-key invariant with byte-equal fast path, fail-fidelity (input bytes back on structural error), provably-safe expect (contains_key guard), preserve_order reliance documented. No bugs.
UNWIRED (FINDING-019 extended): interceptors framework — apply_to_messages has zero production callers (def + re-export + tests only); registration happens only inside a test (astgrep.rs:636). Same class as lifecycle/maturation/registry/units: ported, tested, never invoked. Responses-shape gaps in its helpers moot.
Unwired-subsystem rollup so far: read-lifecycle apply, read-maturation apply, relocate_cache_breakpoint, compressor_registry+apply_strategy, compression_units/batches runtime use, Transform trait, Base split_frozen callers, detection::detect chain, feedback hints consume, interceptors apply. (All documented-as-staged except lifecycle/maturation/protect_tool_results/dead-flags which present as live.)

Next: ctx inject/extract/fetch + memory backend/query/ranker + observability prometheus + openai req/resp/stream + codex/ + vertex/gemini + sse rest + audit/net_offload/display/turn_hooks + compression mod rest.

## Entry 152 — translation layer + ctx inject + prometheus labels
OK openai/request.rs (surveyed): dual-target translators (chat + responses) kept separate deliberately; tool_result → function_call_output / tool-message mapping with block-shape preservation, error markers, image normalization, uncarriable-block placeholders; extensive shape tests. No bugs found in surveyed sections.
MINOR-057: ctx/inject.rs cache_get/cache_put use .expect("inject cache poisoned") — poison crashes the request path; siblings use into_inner(). Low (requires a prior panic while held).
OK prometheus: bounded label vocabularies (model/region/auth_mode/AWS event vocabulary — documented as customer-uncontrolled); Bedrock event_type from parser enum. No cardinality hazard found.
No live-path bugs.

Next: vertex/ + gemini handler core + sse rest (anthropic/chat/framing/outbound/finisher/retry) + openai response/stream + handlers rest + ctx extract/fetch + memory backend/query/decision + audit/redact/headers/health + compression mod/anthropic/model_limits/manifest_totals/cross_turn rest + ctx_offload rest.

## Entry 153 — SSE framing + telemetry discipline
OK framing.rs (core read): zero-copy BytesMut, \n\n + \r\n\r\n terminators, comment/ping skip, parse-vs-forward separation (next_event drops keepalives for state; next_raw_block preserves bytes for forwarding — correct split), no-panic property test. MINOR-058: buffer unbounded — no high-water cap on terminator-less accumulation (trusted upstreams; stall without blank line grows memory).
OK responses machine + telemetry: unknown events warn+preserve with redacted preview; incomplete_reason dual-location tolerance (SDK drift); service_tier on all terminal events.
No bugs.

Next: vertex/gemini + openai response/stream + handlers rest + ctx extract/fetch + memory backend/query/decision + audit/redact/headers/health + compression anthropic/model_limits/manifest_totals/cross_turn-rest + ctx_offload rest.

## Entry 154 — manifest_totals + model_limits + gemini translation
OK manifest_totals (fully read): single-walk fix for provider drift (documents both historical gaps), per-strategy samples, error surfacing. No bugs.
CHECK-022 (new): aggregate() sums tokens/bytes over Compressed blocks ONLY — Outcome.tokens_before/after are block-scoped, not request-scoped; verify savings/conversation consumers don't treat them as request totals.
OK model_limits: vendored LiteLLM table (no startup net), exact-match discipline (documented why not prefix), 128K conservative default, build-time-validated expects. No bugs.
OK gemini translation (core read): preserved-indices mechanism for non-text parts, position-rebuild with fail-open length mismatch (safe direction — originals preserved). JSON-stringify of block arrays in translation is accepted translation loss. No bugs.

Next: ctx extract/fetch + memory backend/query/decision/deferred + audit/redact + compression anthropic/context_editing/cross_turn-rest/ctx_offload-rest + openai stream + handlers rest + sse anthropic/chat/outbound/finisher/retry + bedrock sigv4/eventstream/vendor + vertex raw_predict + foundry + routed/ + codex rate limits + cursor/display/net_offload/turn_hooks/sidecar + config validation + main wiring.

## Entry 155 — redact core (surveyed) + CHECK-023
OK redact (core read): SKIP_KEYS rationale sound (id matching, signature verify, base64 entropy), CLEAR_PREFIXES system paths, 0600 create_new key race handling, walk semantics per shape, restore seam for streams. No bugs in read sections.
CHECK-023 (new): walk_body covers `system` + `messages` only — verify Responses `input`/`instructions` and chat `tools` handling; if redact runs on those shapes unwalked, secrets there miss redaction (and restore seam coverage for their streams).

## Entry 156 — compression/anthropic + context_editing + sse/anthropic defects
OK anthropic.rs: thin policy-boundary wrapper, disabled→0 short-circuit. No bugs.
OK context_editing (fully read): merge-not-clobber, family-aware dedup, clear_thinking keep-overwrite exception (documented Claude-Code inert-keep:all rationale), lead-position API requirement, min-messages payback gate (109k vs 1.8k/turn measured). No bugs.
OK sse/anthropic (core read): defect detector (unterminated/unparseable/missing tool calls — closes the silent-client-reject observability hole), max-merge usage (spec monotone), TTL-split nested read. No bugs.
No live-path bugs.

## Entry 157 — sigv4 + stream accounting + gemini rebuild + translation tests
OK bedrock/sigv4 (core read): delegates to aws-sigv4 crate (no custom crypto), XAmzSha256 forced (gateway 403 lesson), sign-the-wire-bytes contract documented, deterministic-time tests. No bugs.
OK openai/stream (core read): Drop-impl booking safety net (idempotent emit, no-fabrication-when-no-usage guard, provisional-200 terminal guard), deferred CCR booking folded by completion guard. Cost-accounting discipline holds on disconnects. No bugs.
OK gemini rebuild: position-interleave with preserved-originals-wins (safe direction — info preserved, extras dropped). Translation loss (block→JSON-text) accepted + bounded by preserved_indices. No bugs.
OK openai/request+response (surveyed): dual-shape translators with incident-driven tool-call preservation (output[] accumulation fix), extensive shape tests. No bugs found.

## Entry 158 — CTX-2 capture runs cross-provider on Anthropic-shaped identity
FINDING-022 (new): proxy.rs:4424 observes ALL endpoints (anthropic/chat/responses) into CTX-2, but conversation_key hashes system+messages[0] — Responses bodies have neither → session-only key → every Codex turn in a session merges into ONE conversation record; extract_new_messages returns [] (no `messages` array) silently. So Responses capture merges conversations AND records no events. Either gate observe() to messages-shaped bodies or implement Responses identity (instructions + input[0]). CHECK-024 folded into this finding.
OK observer mechanics: byte-budgeted shed-first accounting (burst race documented), per-job panic containment, loud-swallow, powers-of-ten drop reporting. No bugs there.

Next: handlers batch/conversations/stats/local_model/reasoning/route_resolve/count_tokens + memory backend/query/decision/deferred/local + ctx extract/fetch/endpoints/observer/offload_store/projects + observability rest + sse chat/outbound/finisher/retry + bedrock eventstream/vendor + vertex raw_predict + foundry + routed/ + cursor/display/net_offload/turn_hooks/sidecar + config tests + main wiring + bin/ + semantic_cache/memory_tail/subscription/tile_optimizer/tool_schema/tool_search/verbosity/ws_session/ssl/stage_timer/runtime_env/debug/model_router/model_sanitize/project_context/probe/loopback/forwarded_headers/image_decision/injection_budget/health/error/modes/warmup/websocket + audit rest + compression cross_turn-rest + ctx_offload rest.

Next: perpetual — re-verify on new commits; open design questions CHECK-020 (mixed-array markerless drops) + CHECK-021 (top_n unbounded) + CHECK-019 (plan_relevance_split caller map); finding follow-ups F-015/017/018/019/020/022/023 wiring fixes are code changes (out of audit scope until implemented, then re-verify).

## Entry 196 — CHECK-019 RESOLVED (both twins unwired)
Verified by repo-wide grep: content_router::plan_relevance_split callers = its own tests only; relevance_split::* (plan/build/adaptive/segment) = zero external callers. The '\n'-join vs lossless-concat divergence is moot — neither runs in production. Record as unwired duplication (join the FINDING-019-class list); no live impact.

## Entry 197 — prefix_replay line-complete pass (830-1490 read)
OK overlay core (fully read this span): leading-run splice on divergence (revert-lesson documented with 2026-08-09/17 measurements: 25.6% creation recovery), floor-arbitrated inflation bound, shifted-span atomicity, scaffolding-withdrawal alignment, system-adjacency guard, chain-gated replay (continues_chain=false → 0), canonicalizer/churn-blindness with cost-watch tripwire (1.2 ratio). Every decline reason carries cost receipts. No bugs found.
UNWIRED (rollup add): relocate_ephemeral_blocks{,_counted,_reported} — no production callers (self-refs + tests only), consistent with "Relocation is gone" comments. Tested dead code; retention looks deliberate.

## Entry 198 — prefix_replay canonicalizer + slots + tracker ops (100-650, 2125-2344, 2500-3409 read)
OK canonicalizer: same-test-as-overlay (no widening), stored-side-only withdrawal stepping, shrunken-scaffolding superset rule, OPAQUE_PAYLOAD_KEYS collision-awareness (user {"state","index"} data), NON_SEMANTIC_KEYS port-verbatim, string-sugar reminder-filter fix, empty-content boundary agreement, thinking-block 400 guard (8% measured), side-errand parking, slot budget with plan-then-enforce (4-marker limit, 1h-vs-5m billing), offline fingerprints, tail-derived truncation (27-dropped incident fixed), size-predicted alternate retention (log-log r=0.54), chain identity, system-digest adoption gate, credential-hygienic persistence (hash-named files, no key inside, alternates excluded), atomic write-then-rename, head-cache (77ms measured), lock hygiene (no I/O under lock, poison-tolerant).
MINOR-064: stray doc comment 3484-3485 (test-TTL prose adorning history_will_be_rewritten — copy-paste drift).
MINOR-065: stale safety justification 552-555 (cites removed relocate_ephemeral_blocks; conclusion still holds via verbatim-replay but the written reason is wrong).
No logic bugs in ~2900 lines reviewed.

## Entry 199 — prefix_replay breakpoint placement + dead relocation body (1623-2125 read)
OK place_tail_cache_breakpoints (fully read): seal semantics with message-0 exemption (4.4-point measured loss), thinking/proactive-exclusion, shape-stable string wrapping (turn-independent), double-mark counting guard (licence-to-strip correctness), scaffold economics, ephemeral-seal (19% bill), tail-lookback hedge (5%). as_object unwraps provably safe (get() implies object). No bugs.
Note: dead relocate body contains a lossy `no_block_tail` path (spans dropped) — moot while unwired; flag for deletion-or-fix if ever revived (would need the conservation invariant re-proved).

## Entry 200 — prefix_replay COMPLETE (all code ranges read) + CHECK-025
Estimator covers Anthropic + OpenAI-chat shapes; Responses input items uncounted — CHECK-025 RESOLVED as moot: store.complete() gated on Anthropic MessageStop (proxy.rs:10457), Responses never parks → no tracker → frozen 0 → live-zone-only protection by design (consistent with FINDING-022's capture gap: Responses gets neither replay nor capture).
prefix_replay.rs code fully reviewed (1-4225, minus only test modules): no logic bugs. Discipline throughout is the repo's best: every constant measured, every decline named, every guard fail-safe.

## Entry 201 — usage_observer core (attribution + complete + classify)
OK attribution lattice (fully read): exhaustive-enum matching (compile-fails on new variants — incident-driven), ranked client-vs-proxy evidence, edge-triggered-drift blind-spot fix (2026-09-03 incident), replay-decline ranking with measured misattribution correction.
OK complete() (core read): provider-usage ground-truth ledger (not self-reported), savings placement priced by cache boundary (generous upper-bound direction), TTL-split billing, hidden-round splits, stream-aware matching (per-stream priors, unmatched-turn countable-not-silent), stock-arm counterfactual without A/B split (self-validating predicted_read_error).
OK classify_turn (fully read): pure, slack-bounded, fresh-billed-as-waste arm (2026-09-08 lesson), TTL direction. No bugs.

## Entry 202 — fetch_blocks text processing (core read)
OK split_blocks/reassemble (fully read): lossless (raw preserved, exact inverse), fence-aware, heading-delimited, no-empty-blocks. normalize_block_text: conservative (under-match leaks chrome vs over-match hides content — right direction), JS-locale sigma parity documented + implemented. No bugs.

## Entry 203 — audit paths + display provider + warmup (surveyed)
OK audit.rs (fully read): /admin/ prefix + sensitive exact paths, tested incl. slash requirement. No bugs.
OK display_provider.rs (core read): dot-boundary subdomain matching (lookalike-safe), display-only remap (pricing keys internal — spend can't move). No bugs.
OK warmup.rs (surface): load state machine. No flags.

## Entry 204 — dead-flag list grows to 15 (correction to Entry 147 spot-check)
Re-verified: compress_user_messages, compress_system_messages, protect_recent, protect_analysis_context, smart_crusher_with_compaction have ZERO proxy consumers (only bin/headroom_cli/agent_savings.rs re-exports them as env, which nothing reads — flag → env → void). Entry 147's "spot-checked live" was wrong for these five.
Full dead list (parsed, stored, never consumed by proxy flow): read_lifecycle, read_maturation, protect_tool_results, code_aware_enabled, force_kompress_all, lossless, disable_kompress{,_fallback,_anthropic,_openai} (4), compress_user_messages, compress_system_messages, protect_recent, protect_analysis_context, smart_crusher_with_compaction = 15.
(FINDING-018 family. Core struct fields of the same names exist but are only set by profiles/defaults, never from proxy config.)

## Entry 205 — dead-flag sweep complete: 17 total
Systematic check of all remaining bool flags: stateless + offline also dead (zero consumers; comments only). unsafe_allow_unstable_features (rollout snapshot) and no_rewrite_host (folds into rewrite_host) ARE consumed — not dead. All other bools verified consumed.
Final dead list = 17: the 15 above + stateless + offline.

## Entry 206 — openai/stream Responses translation (596-925 read)
OK: delta/done duality with no-delta recovery (message-item loss lesson), refusal-as-text, per-call arg reset, reasoning capture across added/done with signature sealing, truncation-outranks-tool-call, failure→end_turn (half-calls never run), incomplete still books, unknown response.* debug-logged (message-gap lesson). No bugs in read span.
CHECK-026 (new): this translator matches only dotted event names (response.function_call_arguments.* etc.) while sse/openai_responses.rs accepts dotted+undotted — verify upstream convention; undotted variants would drop tool arguments in translation here.

## Entry 207 — stream frame assembly + offload inputs + decision lattices (surveyed)
OK stream 926-1245 (read): abort_terminal deliberately-incomplete close (finisher owns the close), trailing-flush (usage/stop rescue), finished-flag (no re-close loop), error-after-events → no redispatch (no splice risk). No bugs.
OK offload_tool_use_inputs (fully read): mirrors tool_result discipline (live-tail exempt, rebuild boundary, prior-first, tokenizer gate, put-fail warn+skip, namespaced gate keys). No bugs.
OK image/memory/compression decision lattices (surface): shared bypass→disabled→prereq→go shape with skip reasons. No bugs found.

## Entry 208 — bedrock invoke + batch item compression (core read)
OK bedrock invoke (core read): compress-then-sign same-bytes discipline, envelope re-emit defense-in-depth with original-fallback, Drop latency guard, action-from-path (invoke vs converse). No bugs.
OK batch_anthropic compress_batch_item (core read): per-item isolation (malformed → verbatim), synthetic-body dispatch, messages-only extraction (system/tools pass verbatim), ccr tool injection gated on tokens_saved>0. CHECK-027 (new): E1 tool sort applied UNGATED (no PAYG/marker gates unlike request path) — confirm Batch API has no cache-scope semantics needing the marker gate. No bugs found otherwise.

## Entry 209 — vertex forward + anthropic↔openai translation (core read)
OK vertex (core read): envelope-gated 400 (strict by contract), shared anthropic dispatcher, OAuth hard-code (same rationale as Bedrock). No bugs.
OK translators (core read): single/mixed tool_result mapping, error prefixing, assistant tool_calls shaping, single shape switch (no re-derivation). CHECK-028 (new): CHAT translator drops image blocks silently (Responses translator preserves via input_image) — verify intentional vs gap. Thinking-block drop is inherent (no OpenAI equivalent). No other bugs.

## Entry 210 — FINDING-020 extended: memory tool injection shape also Chat-only
Provider enum has no Responses variant; openai_tools() emits nested-function Chat shape. Injected into a Responses request, the shape is wrong for the API (flat name/parameters expected) — upstream may reject/ignore. So memory-on-Responses is doubly dead: injection shape (here) + extraction/execution (Entry 145). Single fix vector: add Responses shaping + parsing to the adapter.

## Entry 211 — ctx inject apply + full gate evidence (executed 2026-09-11)
OK inject apply (read): messages-shape only (consistent with FINDING-022 scope), sentinel-guarded double injection, scaffolding-aware insert position. No bugs.
Gate evidence: `cargo clippy --workspace --all-targets` — 0 errors; warnings style-level + dead-code corroboration (recall_is_empty/hash_value/NullCcrStore never used — matches unwired-subsystem rollup). `cargo fmt --check` — clean (exit 0). Combined with Entry 193 (4491 tests green): fmt + clippy + tests all pass.

## Entry 212 — tool_schema_compaction + cursor bridge (surveyed)
OK tool_schema_compaction (surface read): annotation-key strip list (non-constraints only), first-sentence truncation + semantic-param removal (both opt-in, documented), digest+config-keyed cache. No bugs found at survey depth.
OK cursor/bridge.rs (core read): dual-bound parking (30-min deadline + 32-conversation cap, different axes documented), mismatch circuit breaker (3), chat-id persistence across session churn, documented lock ordering (never hold both — deadlock discipline stated as invariant). No bugs.

## Entry 213 — turn hooks + subscription poll (core read)
OK turn_hooks.rs (core read): panic-contained plugin execution (catch_unwind on sync on_request + async on_response), inert-when-empty, replacement chaining. No bugs.
OK subscription.rs fetch/poll (core read): blocking HTTP on spawn_blocking (runtime never stalls), 401/404 → None not error, shutdown-watch loop with persist-on-exit, join-error warn. No bugs.

## Entry 214 — verbosity AIMD + bedrock auth-mode layer (core read)
OK verbosity_controller observe (fully read): pure AIMD (streak-gated increase, immediate decrease + cooldown, Neutral resets streak), floor/ceil clamps, cooldown decrement-then-set ordering correct. No bugs.
OK auth_mode_layer.rs (fully read): infallible middleware, loud-on-divergence OAuth coercion (no silent fallback), extension contract for downstream, mounted only on Bedrock routes. No bugs.

## Entry 215 — vertex stream re-export + bedrock streaming parity (surveyed)
OK vertex/stream_raw_predict.rs (fully read): documented re-export, no forked logic — single dispatcher + tee flag. No drift surface. No bugs.
OK bedrock invoke_streaming compression (core read): mirrors invoke exactly (OAuth hard-code, None CCR store, envelope re-emit, dedup post-pass). No forked logic to drift. No bugs.

## Entry 216 — cursor handler resume + memory save validation (core read)
OK cursor/handler.rs handle (entry read): parked-driver resume, mismatch circuit breaker (orphan kill + fresh start, 3-strike error with expected/got). No bugs found.
OK execute_save (core read): empty-content rejection, scope/project contradiction rejected loudly, mistyped project path fails loudly (not silent unreachable partition), resolver-based scoping (no cwd disagreement). Sound input validation. No bugs.

## Entry 217 — ctx inject decide (core read)
OK decide() (fully read): replay-verbatim on decided (I4), race-aware row-miss (strictly-behind-turn proof vs capture race), never-eligible vs row-miss distinguished (decision-free either way), too-deep-for-first-sight decline (cache-prefix protection), build+persist with failure warns. Every decline named and measured. No bugs.

## Entry 218 — memory search + GCP ADC cache (core read)
OK execute_search (core read): advertised-but-unread entities filter wired (2026-08-26 fix), overfetch-before-filter (filter narrows corpus, not pre-ranked top-k). No bugs.
OK vertex/adc.rs (core read): refresh-ahead expiry, retry-on-transient via Mutex<Option> (not OnceCell — no error caching), sub-µs hit path. Sound credential caching. No bugs.

## Entry 219 — route resolve + conversations + response translation (core read)
OK route_resolve (fully read): cursor-first, route-table second, no-match third; tested (cursor-beats-URL, first-match-wins, unknown-no-match). No bugs.
OK conversations forward (entry read): instrumented passthrough with compression explicitly deferred (C5+), gated redaction seam. Documented scope. No bugs.
OK openai_to_anthropic_response (core read): refusal-as-text, array-content recovery, identity-less tool call isolated alone (doesn't take text down). No bugs.

## Entry 220 — codex token refresh (core read)
OK refresh flow: re-reads file per call (picks up CLI rotation), rotation-aware (updates rt/idt when present), persist-failure warns but returns token (degraded, not dead), refresh-failure None (caller falls back). No auth-logic bugs.
MINOR-066: auth file rewrite is non-atomic (direct fs::write of read-modified JSON) — crash mid-write corrupts credentials; prefix persistence uses write-then-rename, this path doesn't. Low (refreshes rare, but the file is critical).

## Entry 221 — ws pump + tool_schema cache (core read)
OK websocket pump (fully read): bilateral cancel-token abort (half-close hang fix), tungstenite-managed headers skipped, subprotocol validated. No bugs.
OK tool_schema cache (core read): FIFO-8, canonical-JSON digest (sorted keys — insertion-order stable), poison-tolerant locks, invalidate hook, properties-children never dropped. No bugs.

## Entry 222 — cursor agent lifecycle (core read)
OK agent.rs (core read): tempdir workspaces, absolute turn deadlines (talkative-agent lesson — per-read timer would never trip), sandbox-vs-ask lesson documented, stub-testable binary path. No bugs.
OK turn.rs next_step (core read): biased select (parked-before-output — reproducible frame order for tests). No bugs.
OK endpoint.rs (surface): MCP JSON-RPC surface with conversation isolation + readable errors. No flags.

## Entry 223 — probe recorder + TLS bundle loader (core read)
OK probe_recorder (fully read): 0700 dir perms, never-raises record, env-gated construction. No bugs.
MINOR-067: ssl bundle splitter silently skips individually-unparseable certs (errors only on zero-valid); a corrupt cert among valid ones vanishes without a log. Low (file-level load warn exists).

## Entry 224 — cursor handler drive + debug introspection (surveyed)
OK cursor/handler.rs drive/collect (core read): single driver for stream+collect (no transport fork), unknown events skipped (not fatal), disconnect reaps parked driver (no orphan). No bugs.
OK debug_introspection (surface read): aggregate counts only, project paths loopback-gated. No content leakage into snapshots. No bugs.

## Entry 225 — forward_http entry + plumbed-but-unconsumed policy fields
OK forward_http entry (core read): extension-before-body-take (no header leak upstream), single auth classify + policy stash (no per-stage recompute), enforcement-disabled→PAYG with dashboard split, entry log with best-effort bytes. No bugs.
MINOR-068: CompressionPolicy volatile_token_threshold/max_lossy_ratio flow proxy→handlers→transforms but are read only for log lines (proxy.rs:3705-3706) — F2.2 tuning surface pending, control unwired.

## Entry 226 — forward_http dispatch + key derivation (4035-4200 read)
OK dispatch (core read): single-source endpoint match, Anthropic-only sanitizers gated in lockstep with dispatch, sidecar short-circuit, billing-header pin before fingerprinting, Bedrock drift-detector skip (shape mismatch rationale). No bugs.
OK key derivation (core read): derive-once pre-mutation shared by session/lane/conversation (re-derivation trap documented), volatile detector shape-dispatched per endpoint (OpenAiResponses covered — narrows redact-shape concerns to whole-body redact only). No bugs.

## Entry 227 — ctx fetch redirect chain + search endpoints (core read)
OK get_url (fully read): manual redirect walk with per-hop ssrf_check (redirect-rebind hole closed — documented why reqwest follower disabled), scheme gate on Location, hop cap, body cap. `unreachable!` provably holds (bounded loop, all arms return/continue/err). No bugs.
OK ctx/endpoints.rs request_project (core read): header-derived project scoping with shared-bucket fallback, 503 on unopenable store. No bugs.

## Entry 228 — ctx observer worker + offload gate persistence (core read)
OK observer start (fully read): per-job panic containment (documented why no thread restart), budget-refund-on-receipt (burst-race correct), per-project failure isolation (one DB down doesn't stop others). No bugs.
OK gate hydrate/record/seed (core read): hydrate-once + write-outside-lock, birth-only seeding with live-session refusal (empty-set trap documented), truncated donor sets, TTL-filtered restores. Mirrors replay persistence discipline. No bugs.

## Entry 229 — sidecar metric kinds + ctx_backend search (core read)
OK sidecar metric (verified): observe_detected called with const kinds only (DESCRIBE_ACTION_KIND, FALLBACK_KIND) — no cardinality hazard. No bugs.
OK ctx_backend search_memories_sync (core read): empty-query enumeration path (BM25 can't match nothing — records instead, score floor documented for min_similarity filters), narrow-then-wide overfetch (partition-filter ordering fix), related-fill for empty slots only. No bugs.

## Entry 230 — model-label cardinality cap (core read)
OK proxy_counters bounded_model (fully read): 1024-distinct-model cap with "other" bucket + warn-once, membership-tested-before-insert (no self-defeating admit), applied before counter AND timing histograms. The exact hostile-client cardinality hazard is handled. No bugs.

## Entry 231 — openai/stream block discipline (159-215, 507-596 read)
OK: open_block no-op-same-kind, open_tool_block always-closes (consecutive calls separate), first-choice-only (n>1 skipped upstream), id-guard preserves across chunks, close_block_final index discipline.
MINOR-069: args-before-id chunk order emits a phantom empty-id tool_use block (open_block with "" identity, then open_tool_block re-opens). Upstream spec sends id first — unreachable in practice; defensive path only.

## Entry 232 — bedrock streaming tee + vertex verbs (core read)
OK invoke_streaming tee (core read): bounded try_send (never blocks byte path), shared AnthropicStreamState (usage parity with direct path), warn-on-apply-error. No bugs.
OK vertex/mod.rs verbs (fully read): closed enum, unknown → warn + 404 (never silent default verb), last-colon split. No bugs.

## Entry 233 — routed booking-once + prepare ordering (core read)
OK response_arms BookingStream (core read): books exactly once (booked flag + completion + Drop fallback), non-CCR passthrough untouched. No bugs.
OK routed/prepare.rs prepare_turn (core read): documented stage choreography (capture → shed on pre-transform identity → lane pins → holds on Anthropic shape → redact after readers/before forwarders → compress/offload/replay → translate). Ordering rationale stated per stage. No bugs.

## Entry 234 — routed resolver alternation + F-020 confirmation
OK routed/ccr.rs resolve_routed_proxy_tools (core read): bounded alternation loop (fixpoint-or-cap) so cross-resolver continuations (memory→retrieve chains) always get a runner. MAX_RESOLVER_ALTERNATIONS bounds it. No bugs.
FINDING-020 CONFIRMED FINAL: proxy.rs handle_memory_response IS items_field-aware (input vs messages) but its detection gate (12461, 12529) calls handler.has_memory_tool_calls → adapter get_tool_name → "" for Responses function_call items. The provider-aware shaping downstream never runs because the gate fails first. Responses memory dead at the gate on all paths using this handler.

## Entry 235 — routed translation pipeline + stream test coverage (surveyed)
OK translation.rs translate_routed_request (core read): single shape decision (no re-derivation), post-translation cache-key injection (right shape home, PAYG-gated, subscription carve-out documented), Responses stream forcing. No bugs found.
OK openai/stream tests (surface): truncation, signature edge cases, reasoning/function-call translation, envelope round-trip — covers the verified paths. No gaps noted.

## Entry 236 — config route parsing + StageTimer soundness hole
OK parse_model_route (core read): strict errors (no silent misparse), cursor: transport, auth_env split, translate keywords. No bugs.
MINOR-070 (new, soundness): StageMeasurement holds a raw *mut with NO lifetime param — measure(&mut self) return does NOT borrow-check the parent-outlives-guard contract despite the docstring's claim ("guaranteed by the borrow checker" is false). Move/drop of StageTimer while a guard is alive = UAF write. Currently ZERO callers (dead API) — fix with PhantomData lifetime or remove before wiring.
OK sqlite_tuning FFI (read): call_once + documented silent-decline direction. No bug.

## Entry 237 — docs actively document dead flags (doc-code drift)
docs/flags.md (generated from --help) presents --code-aware, --disable-kompress{,-fallback,-anthropic,-openai}, --lossless, --read-lifecycle, --read-maturation, --protect-tool-results as working operator controls; --exclude-tools text reads provider-global. All 17 dead flags + FINDING-017's scoping gap are thus documented as functional. The drift originates in clap help text + missing wiring. Fix vector: wire, remove, or annotate (help text is the cheapest correct fix).

## Entry 238 — ctx_backend ranked search + metric vocab validation (core read)
OK ranked_for_user (fully read): bilingual interleave (trigram-starvation fix measured), best-hit-per-memory + content dedup (duplicate-text incident), orphan tolerance (crash-safe), partition visibility (own + shared). No bugs.
OK metric_names service_tier::validate (read): bounded vocabulary with loud-on-drift bucketing. Consistent label hygiene. No bugs.

## Entry 239 — memory continuation loop + sentinel splicing (core read)
OK handle_memory_response rounds loop (core read): per-round retry (429/5xx + transport only), failed continuation preserves built body (no drop), SSE-fold for streaming backends, stranded-call trace reporting. No bugs in loop mechanics.
FINDING-020 corroborated from the far side: memory_results_message + extend_or_push ARE Responses-aware (`_openai_responses_tool_results` sentinel splicing) — the entire continuation plumbing handles Responses except the adapter's detect/format fns. Single-point failure confirmed.

## Entry 240 — stream_finisher tail synthesis (core read)
OK tail() (fully read): discarded-tool naming (repairable vs finished misread), stop_reason rewrite (phantom tool_use refusal), withheld-index reuse (never reached wire), universal end_turn. No bugs.

## Entry 241 — bedrock translate_stream + vertex dispatch (core read)
OK translate_stream (core read): synthetic ping first (wire parity + steering arming, unconditional per Python reference), drain-before-read loop, bounded-vocabulary metric label. No bugs.
OK vertex dispatch (core read): verb-gated 404s (never silent default), last-colon split (model ids contain @), single shared forwarder. Client x-request-id trusted for correlation (standard trace practice). No bugs.

## Entry 242 — foundry handler + routed response arms (core read)
OK foundry handle_foundry_messages (fully read): path normalization, upstream override via extensions, gated redaction seam with edge restore. No bugs.
OK routed/response_arms read_routed_body + fold_buffered (core read): error bodies booked + warned (not silent), shared fold across shapes with per-shape logs. No logic bugs.
MINOR-071: upstream error body logged in FULL at warn (local_model_upstream_error) — may echo sensitive request content; elsewhere previews cap at 96 chars. Inconsistent log-hygiene.

## Entry 243 — routed sidecar + model routing (core read)
OK routed/sidecar.rs (core read): measured budget/effort shaping (reasoning-token starvation fix), fail-fast single attempt (no retry — Haiku fallback window), stateless (no per-conversation state, invariant upheld). No bugs.
OK routed/routing.rs apply_model_routing (core read): cooldowns consulted on both paths (failed targets passed over), identity_model preserved for downstream keying. No bugs.

## Entry 244 — new-commit review (HEAD 31c99a07 + NO-OP annotation 51f0e00c)
OK HEAD live_zone delta (fully read): kompress_status() pure reader of existing statics (matches health.rs soft-readiness use verified in Entry 181). No load trigger, no behavior change. No bugs.
OK HEAD scope cross-check: cold-prefix fork (maybe_cold_fork verified), tool_search gate (verified), ccr_retrieve repair (verified), savings ledger fields (test-green), install defaults + wiring slices — all land in files already verified at their post-commit state (all reads this session were post-HEAD worktree).
Dead-flag update: 51f0e00c annotates min-tokens-to-crush + max-items-after-crush as NO-OP in help (mitigated, not fixed) + idea file dead-crush-flags.md. Numeric/string sweep adds nothing further (cors field doesn't exist; rest consumed). Final dead list = 19 (17 + 2 annotated).

## Entry 245 — CCR keyword query path commit a6e8f6d0 (diff-reviewed)
OK schema (4 definition sites): query property added, required dropped, exactly-one documented. OK parse: trim/empty/2000-char bound, hash-wins precedence, overlong→malformed-but-answered (never leaked to client). OK execution: spawn_blocking search, 80-char log preview, redaction parity with hash hits, top-N inline. Tests pin all three parse arms. No bugs.

## Entry 246 — recent fix commits (message-reviewed + spot-verified)
OK 6c2c4bd2 stage timings: observation-only (null placeholders on early return), gated rewrite decision (≥20ms p50). No wire/control change by construction. No bugs.
OK 2914e9ac lineage bounds: unbounded HashMap → bounded LRU, expect() → exhaustive IdentityBranch enum (audit-responsive; matches drift_detector.rs state verified). No bugs.
OK 2dc8a574 cursor resume logging + mismatch cap: verified live in handler.rs (Entry 216 mismatch breaker). Consistent.
Process note: commits reference audit findings and close them with measured rationale — the audit loop is functioning as designed.

## Entry 247 — CCR handler IS Responses-aware (memory adapter is the outlier)
OK core response_handler extract/parse (core read): full openai_responses arm (output[] flat function_call), call_id mapping (vs id), top-level name checks across anthropic/function/functionCall shapes, invalid-hash-stays-CCR-side (no clean-resolution lie), residual-status taxonomy (#839 intentional-skip distinction). No bugs.
FINDING-020 sharpened: CCR retrieval handles Responses end-to-end; ONLY the memory tool adapter (proxy) is Chat-shaped. Fix template exists in-tree (mirror these provider arms). Scope of fix confirmed minimal.

## Entry 248 — CHECK-027 + CHECK-028 resolved (both real)
MINOR-072 (was CHECK-027): batch outer E1 sort (batch_anthropic.rs) runs UNGATED — no PAYG gate, no marker gate — while the request path gates both. The inner dispatcher re-checks markers (no corruption), but customer-placed cache_control scope still shifts within batch items. Narrow (batch + markers + order-dependence), but real. Fix: apply the same gates.
FINDING-024 (was CHECK-028, medium): chat translator (openai/request.rs translate_user_message) silently drops image/document blocks (`_ => {}`); an image-only user message emits NOTHING — content vanishes without a trace. Responses translator preserves via input_image. Routed/local-model path only (main Anthropic passthrough unaffected), but silent data loss where it hits.

## Entry 249 — assistant translator (same file, same family)
FINDING-024 extends: translate_assistant_message drops thinking (inherent — no OpenAI equivalent) AND image blocks identically. Same scope/grade.
MINOR-073: request translator emits id-less/tool-less tool_calls verbatim (no isolation); response path isolates broken calls alone. Garbage-in direction only (client sent it), but asymmetric robustness.

## Entry 250 — count_tokens handler + estimator (core read)
OK estimator (fully read): tool_use input counted (model must reproduce), recursive tool_result, tools JSON included, image exclusion documented (pixels billed by size — honest limit). No bugs.
OK handler (core read): local estimation ONLY where upstream can't count (cursor subprocess, translated routes); everything else forwarded byte-identical for exact counts. Never fabricates where upstream is authoritative. No bugs.

## Entry 251 — stats handler scoping (core read)
OK handle_stats (core read): per-request rows gated by dashboard auth (aggregates public — upstream include_sensitive split ported), saturating tally arithmetic, wire-verdict framed honestly ("everything above measures the proxy against itself"). No bugs.

## Entry 159 — memory query construction gap
MINOR-059: query.rs from_messages ignores text blocks in array-form user messages when building user_text (only String content + tool_result blocks collected). Block-form clients' actual questions miss the embedding query → recall quality gap (not a correctness bug; no crash/wrong-forward).
No live-path bugs.

## Entry 160 — deferred memory placement discipline
OK deferred.rs (core read): TTL + MAX_HELD bounds, TurnNotHere/Unusable/Done placement lattice, prove-after-write with undo (refuses to send API-refusable bodies), drop-answer-over-corrupt-turn priority. Exemplary fail-safe ordering. No bugs.

## Entry 161 — loopback guard + call sites
OK loopback_guard.rs (fully read): mapped-v6, bracket/port parsing, IP-literal requirement (DNS-rebinding rationale), tested incl. adversarial hostnames. No bugs in the module.
MINOR-060: is_loopback_host(None)==true (documented TestClient/UDS convenience) — grants loopback when peer addr unavailable. Call sites: proxy_auth/main pass Some (safe); websocket origin parse-fail denies first (host None practically unreachable for gated schemes — negligible); proxy.rs:1628 debug-guard passes Option ConnectInfo (absent → Gate 1 passes; Gate 2 Host check remains; TCP always provides ConnectInfo; UDS implies local). Low overall.

## Entry 162 — SSE outbound/framing-split + stream retry + translators
OK sse/outbound (surveyed): single-source Anthropic frame constructors (dedup-by-design, event/type agreement enforced by construction). No bugs.
OK stream_retry (core read): hold-back-uncommitted semantics (2 KiB observed-drop window vs TTFB tradeoff documented), retry only pre-commit (no splice risk), wraps below CCR rewriter + telemetry (discarded attempts invisible downstream). No bugs.
OK openai/stream + response translators (surveyed): Drop-booked cost accounting, output[] accumulation (tool-call-vanish fix), shape tests. No bugs found.

## Entry 163 — Bedrock EventStream parser
OK eventstream.rs (core read): plausibility-before-buffering (implausible prelude fails fast, no multi-GB wait), max_message_bytes cap, prelude+message CRC validation (operator-degradable only for debugging), length-guarded expects (provably safe post-checks), property-tested no-panic. Exemplary binary parsing. No bugs.

## Entry 164 — routed/ transforms (surveyed)
OK routed/transforms.rs (core read): stage ordering mirrors Claude path (volatile-detect → session/lane keys → drift observe → gate seeding → offload), lane_key isolation for sibling streams (item-11 lesson applied), identity_model keying for reroutes, shared transform labels for /stats parity. Consistent cross-path architecture. No bugs found in read sections.

## Entry 165 — reasoning signature envelope
OK reasoning_signature.rs (core read): stateless envelope beats cache-key design (documented failure modes of the alternative), bounded sizes (4K id / 8M blob), prefix namespacing (foreign envelopes → None, dropped not replayed), decode-vs-recognize split (Anthropic-refusal semantics), split_once unambiguous (base64url id can't contain ':'). No bugs.

## Entry 166 — ctx/fetch SSRF guard gaps
FINDING-023 (new): ssrf_check hand-rolled octet checks miss two localhost-reaching families: (1) 0.0.0.0/8 (unspecified — octets[0]==0 matches no arm, yet routes to localhost on typical stacks); (2) IPv4-mapped IPv6 (::ffff:127.0.0.1, ::ffff:10.x — segs[0]==0 matches no v6 arm). Fix: use std's is_unspecified/is_loopback/is_private (+mapped handling via to_ipv4_mapped) instead of hand-rolled ranges. Also absent: 100.64/10, 192.0.0.0/24 (minor).
MINOR-061: check-then-fetch TOCTOU (DNS rebind between lookup_host and connect) — standard limitation; accept or pin with connect-time validation.
OK otherwise: scheme gate, per-answer IP walk, tested loopback/private/public arms. No other bugs.

## Entry 167 — cache-hit-rate metric discipline
OK cache_hit_rate.rs (core read): H2 clean-completion gate (no half-stream garbage), NaN loud-skip (M3: clamp returns NaN in release — good catch), bounded provider labels, denominator definition sane (read/total incl. creation). No bugs.

## Entry 168 — tile_optimizer + tool_search gate (surveyed)
OK tile_optimizer (core math read): provider scaling formulas, tile-boundary search with 40% pixel floor, u32 ranges safe for realistic dims (sub-pixel → 0 dim degenerate only). No bugs.
OK tool_search_deferral gate (core read): lookalike-safe host extraction (api.anthropic.com.evil.com → custom), client-mechanism detection (typed vs name-prefixed distinction for stale-history safety). Truthy set {1,true,yes,on,auto} default-ON — another FINDING-011-family variant (cold_recompact explicitly excluded on/auto; documented there).
No bugs.

## Entry 169 — FINDING-011 CLOSED (confirmed): env-truthiness inventory
A canonical parser EXISTS (config::parse_bool_flag: {1,true,yes,on}/{0,false,no,off}, strict Err) but only clap CLI parsing uses it. Every hand-rolled env reader diverges:
A. cold_recompact {1,true,yes}: "on"→OFF (fail-open for a safety feature — worst direction).
B. freeze_block_decision {1,true,yes,on}.
C. protect_reads NOT-{0,"",false,no}: "off"/typos→ON (fail-safe), ""→OFF.
D. lossless_compaction None→ON; falsy {0,false,no,off}.
E. tool_search {1,true,yes,on,auto} default-ON.
F. kompress must_keep exact-untrimmed-"0" only.
G. syntax_breaker NOT-{0,false,off}: "no"→ON.
H. read_protection see C.
No data-loss; usability-trap class with one fail-open case (A). Fix: single env_bool helper used everywhere (parse_bool_flag already exists).

## Entry 170 — injection budget (core read)
OK injection_budget.rs: non-clippable recall charged whole with overrun-to-zero (byte-stability beats budget — I4 rationale documented at the decision point), clippable cut at line boundaries, zero-budget legitimate-off, request-correlated constructor for live paths. Sound. No bugs.

## Entry 171 — codex rate limits + model router (surveyed)
OK codex_rate_limits (core read): merge-not-replace across header/stream timing, poison-tolerant locks, multi-depth extraction (upstream drift tolerance). No bugs.
OK model_router (core read): first-match-wins + skip-veto (cooldown without clock coupling), never-fails passthrough, Python-format parity in reason strings, system-inclusive token estimate (documented why). No bugs.

## Entry 172 — tail-injection triple implementation (unwired, drifting)
THREE implementations of append-memory-to-latest-user, ZERO production callers combined:
1. memory/handler.rs::append_to_latest_user_tail — MINOR-056 destructive OpenAI branch (replaces array content).
2. memory_tail.rs::append_to_latest_user_tail — sound (double-inject guard, frozen floor, delegates OpenAI to body.rs).
3. body.rs splice_latest_user (+Responses input_item analog) — correct shape-aware splice, called only by (2).
Fix direction: keep body.rs, delete (1)+(2) or wire (2). Drift already visible ((1) vs (2) disagree on OpenAI arrays). No live impact (all unwired).

## Entry 173 — semantic cache locking (sound)
OK semantic_cache.rs: sync get/set (no await under std RwLock — no executor blocking), refcounted bodies (measured memcpy win), LRU with preserved eviction semantics + logging, TTL expiry, Responses-aware key inputs (messages/input/bare-string). Poison-unwrap present but sections panic-free in practice. No bugs.

## Entry 174 — forwarded headers + dashboard auth (sound)
OK forwarded_headers.rs (core read): XFF honored only from trusted-gateway CIDRs (no blind trust), same-origin-or-no-provenance (CLI absence valid, browser presence must match), dashboard metadata triple-gate (loopback/gateway + IP-literal Host + CIDR allowlist, strict-empty default). Sound posture. No bugs.

## Entry 175 — memory query/ranker/decision + ctx projects (surveyed)
OK query_translation (read): measured RU→EN stem augmentation (augment-not-replace, longest-stem, phrases-first). No bugs.
MINOR-062: ranker parse_timestamp docstring claims ISO-8601 support; impl returns None for all non-numeric (harmless — None = neutral rank — but doc misleads).
OK decision.rs: clean inject lattice with skip reasons. No bugs.
OK ctx/projects.rs (core read): hash-named DB files (traversal-safe by construction), LRU-capped handles, fail-to-None safe direction, one-shot sweep handles, cross-project recall incident documented + fixed. No bugs.

## Entry 176 — bedrock vendor + sse/openai_chat (core read)
OK vendor.rs (fully read): closed geo-prefix set (unknown stays own vendor), ARN converse-route forcing, tested incl. negative vendors. No bugs.
OK sse/openai_chat.rs apply (fully read): index-keyed choices + tool calls (P1-17 lesson applied here too), first-write-wins id/name, missing-index warn+drop (never silent choice[0] fallback), unexpected event: line surfaced. No bugs.

## Entry 177 — routed retry + quirks (surveyed)
OK routed/retry.rs (core read): shared bounds with Claude path (one meaning for retry flags), 401-refresh outside budget (credential fix ≠ transient failure). No bugs.
OK routed/quirks.rs (surface read): upstream classification + URL shaping helpers. No bugs found.

## Entry 178 — local_model handler + config posture
OK handlers/local_model.rs handle_messages (entry read): inflight drain guard (conservative overcount documented), shared request-id across outcome+replay, non-JSON delegation, sidecar-before-routing ordering (documented why). No bugs.
Note: config.rs has no central validate() — invalid combos degrade per-subsystem with loud warns (consistent fail-open philosophy, observed at CTX-2/3 startup). No silent misconfig found; dead flags (Entry 147) are the sharp edge here, not validation.

## Entry 179 — observability metric vocabulary (surveyed)
OK proxy_metrics.rs (core read): bounded label vocabularies (path endpoint set, retry_reason consts incl. 529 split + in-band-SSE), shared vocabulary between retry/exhausted (divisible), counter-next-to-sleep placement (anti-drift), C2 passthrough-modified alarm wired. No cardinality hazard found. No bugs.

## Entry 180 — foundry + vertex envelopes (core read)
OK foundry/mod.rs (core read): resource→hostname derivation with parse-validated fallback (invalid → warn + --upstream, no boot failure), query-preserving path rewrite, shared forward_http pipeline (no forked compression path). No bugs.
OK vertex/envelope.rs parse (fully read): rejects body-model (URL-carried by Vertex contract), stringifies-but-flags version drift, has_messages gate. Sound strictness profile. No bugs.

## Entry 181 — routed retry/quirks + health + modes (surveyed)
OK routed/retry.rs: shared bounds, 401-outside-budget. OK quirks: classification helpers. OK health.rs (fully read): kompress soft-excluded from readiness (degrade≠fail), livez previously-forwarded bug fixed, absolute-path upstream health (RFC 3986 join trap avoided). OK modes.rs (fully read): alias normalization with default fallback. No bugs.

## Entry 182 — net_offload + sidecar + runtime_env (surveyed)
OK net_offload (read): WSL2 TLS-corruption startup diagnostic, route-table NIC lookup (assumed-eth0 incident), warn-only posture (no root actions under sessions). No bugs (read-only + warn).
OK sidecar (core read): spinner short-circuit with API-valid tail trim (unpaired tool_result→text, thinking dropped with signature rationale, per-block caps). No bugs.
OK runtime_env (core read): hot-reload allowlist (5 knobs; rest restart-bound by design), test-serialization lock (intermittent-failure lesson), poison-recovered test lock. No bugs.

## Entry 183 — cursor translators + ws registry + ssl (surveyed)
OK cursor/translate+turn (surface read): third SSE state machine (parked tool_use, pause_for_tool, finish_unterminated) — same disciplined family. No bugs found at survey depth.
OK ws_session_registry (core read): idempotent register/deregister, saturating task accounting. No bugs.
OK ssl_context (core read): strict-by-default TLS with explicit opt-out list, replacement-vs-additive CA semantics, missing-file warn (not silent). No bugs.

## Entry 184 — websocket URL + local memory search (surveyed)
OK websocket.rs build_upstream_ws_url (read): scheme gate (http/https/ws/wss only), path+query join. No bugs.
OK local_backend search (core read): user-scoped + current-only filter, score>0 gate, stable sort. Substring (non-boundary) matching is recall-roughness, documented as basic. No correctness bugs.

## Entry 185 — metric label cardinality (surveyed)
OK: tool names bucketed pre-label (ctx_offload_by_tool::bucket), retry reasons const-vocabulary, strategies/content-types enum-bounded, ccr_splice reasons const-vocabulary. No unbounded client-controlled labels found. No bugs.

## Entry 186 — main startup hygiene + offline binaries (surveyed)
OK main.rs (entry read): ordered init (sqlite-before-anything, tracing, ORT, route warnings, NIC check, identity log with binary len+mtime, beta-sticky-inactive warn, license-key-ignored warn, non-fatal bedrock creds, off-path kompress warm). Every silent-no-op condition warns loudly at startup. Exemplary. No bugs.
OK bin/ (surface survey): offline analysis tools (offload_sim grid sweep, prefix_replay_rate corpus measurement, headroom_cli audit/tools/auth/network_diff) — not hot path. No audit flags.

## Entry 187 — error taxonomy + project context (core read)
OK error.rs (core read): 503+Retry-After for transients (client-retry-friendly, x-headroom-retryable marker), 502 for non-retryable, Config fatal-at-construction with fix instructions. Sound taxonomy. No bugs.
OK project_context.rs (core read): thread-local project binding, printable-ASCII + 200-char sanitization on all ingress (header, path segment). No injection path. No bugs.

## Entry 188 — fetch_pages keys + observability label hygiene (surveyed)
OK fetch_pages helpers (read): fragment-stripped page identity (self-comparison fix), upstream-mirroring host serialization (bracket restore). No bugs.
OK observability (surveyed): outcome/reason/stage labels come from code constants, "never from request input" (documented at ccr_retrieval). Consistent with Entry 185. No bugs.

## Entry 189 — routed auth + outcome accounting (surveyed)
OK routed/auth.rs (core read): missing/empty credential → loud 500 (not silent 401-chase), token trimmed + header-validated, `none` escape hatch documented. No bugs.
OK routed/outcome.rs (core read): upstream-model booking (alias-mispricing trap named), provider-label parity with forward_http, OpenAI-vs-Anthropic usage convention documented (double-count trap named), session_key dead field documented inline. No bugs.

## Entry 190 — routed ccr/redaction surface + cross_turn tests + CLI tools (surveyed)
OK routed/ccr.rs (surface): retrieval/memory/proxy-tool resolvers with redaction-before-continuation tests. OK routed/redaction.rs: outbound redact + buffered/streaming restore. OK cross_turn.rs tests pin flag-off/mode-off/earliest-untouched/marker-target/no-dup-no-change. OK bin/ CLI (surface): offline analysis tools, not hot path. No bugs found.

## Entry 191 — misc small modules (surveyed)
OK warmup/subscription/turn_hooks (surface): state holders, poll loop, hook registry. No bugs found.
OK offload_tool_result (core read): I2→I3→prior→J4→structural/preview→I6 gate lattice, shape-preserving replace, gate record for monotonicity. Each layer documented. Token (not byte) gate — byte growth theoretically possible via tokenizer quirks; accounting is token-based so consistent. No bugs.

## Entry 192 — config parse fns + PyO3 bridge
MINOR-063: parse_bedrock_model_map silently skips malformed pairs (no `=`) — operator typos vanish without a log; contrasts the codebase loud-failure norm.
OK headroom-py bridge (surveyed): GIL released on heavy paths (compress/crush/detect), synthesized chain-confidence documented (1.0 + empty metadata, no readers today). Nuance to INCONSISTENCY-010: the bridge exposes the magika+unidiff CHAIN to Python while the Rust hot path runs the oracle — cross-language verdict split by construction.
No live-path bugs.

## Entry 193 — test evidence (executed 2026-09-11)
`cargo test -p headroom-core --lib`: 2173 passed, 0 failed, 2 ignored (1.73s).
`cargo test -p headroom-proxy --lib`: 2318 passed, 0 failed, 1 ignored (31s).
Combined 4491 green corroborates the audit: no live-path logic bugs found in verified modules; pinned-buggy-behavior tests (split_frozen_all) pass as documented.
Parity crate (surveyed): fixture-comparator harness — the mechanism behind all byte-parity pins. No flags.

## Entry 194 — contrib scripts + simulators + core layout (surveyed)
OK restart-headroom.sh (read): fate-sharing avoidance (setsid/nohup), rollback on failed health, flags-file gate (refuses defaults), lsof/ss portability, watcher preservation. Careful ops tooling. No bugs.
OK headroom-simulators (surface): standalone deterministic provider mocks for CI (never call real LLMs). No audit flags.
OK core lib.rs/transforms mod.rs (surface): full module registry reviewed across entries 120-139. No missing-module flags.
Scope note: docs/notes (working notes, some stale per AGENTS.md — not spec, not audited line-by-line), Python mirror tree (read-only per AGENTS.md — not audited), e2e/ absent. Everything else in-repo covered at deep (hot paths) or survey (ports/fixtures/ops) depth as recorded per entry.

## Entry 195 — CHECK resolutions vs upstream-python mirror
Structural discovery: upstream-python transforms are RETIRED shims delegating to Rust via PyO3 (diff_compressor Stage 3b, smart_crusher Stage 3c.1b — verified in file headers). "Python parity" = migration locked by fixtures, not a live second implementation.
CHECK-013 doubly-CONFIRMED: Python content_router.py:4085 skips split over max_records (keep-all) — matches Rust + test pin.
CHECK-014 RESOLVED: Python has live backend selection; Rust resolve_detect_backend has zero consumers (per-site hardcoding) — vestigial.
CHECK-015 RESOLVED (moot): single implementation (Python delegates to Rust) — quoted-path gap, if real, bites both identically; no divergence possible.
CHECK-016 RESOLVED: Python _DIFF_HEADER_PATTERN byte-identical in coverage (no `--- foo`, no count-less `@@`) — parity-bound gap for non-git diffs, same both sides.
CHECK-020 REFRAMED (still open as Rust-behavior question): no live Python to compare; markerless group drops contradict the CCR contract on their own terms — needs design-intent answer, not parity.
CHECK-021 REFRAMED (still open): same — top_n unbounded-additive needs retired-source archaeology (git history) or intent ruling.
CHECK-022 RESOLVED: block-scoped sums are self-consistent (absolute savings correct; ratios within-scope; gated >0; not presented as request totals).
CHECK-023 RESOLVED (by-design scoped): whole-body redact_body is routed-Anthropic-only; main paths use shape-agnostic redact_string/redact_value on continuation contents. No evidence of a hole; exhaustive per-provider flow coverage would be a follow-up.

## Entry 151 — memory tail injection: destructive branch in unwired helper
MINOR-056: handler.rs append_openai_tail non-string branch (1563-1566) REPLACES array content with bare context_text (destroys original blocks; also wrong shape for Responses message items). Untested (tests cover string + anthropic-array only) and currently zero production callers — must fix before wiring. Anthropic side correct incl. frozen floor + never-empty + regression-pinned caller bug (1911).
No live impact today.

## Entry 131 — adaptive_sizer.rs (693, code fully read) + cold_prefix.rs (836, code fully read) + cross_turn_dedup.rs (969, code fully read)
OK adaptive_sizer: 3-tier compute_optimal_k (fast path clamps honoring caller cap, simhash-redundancy, Kneedle + diversity floor, bias truncate-toward-zero like Python int(), zlib validation with 200B small-skip), find_knee (flat→Some(1) Python-literal, strict 0.05, 1-indexed), simhash (MD5-first-64BE = int(hex[:16],16), per-codepoint grams, short-input single-iteration, lowercase parity), bigram curve (CJK char-bigram synthesis), greedy clustering. 38 tests incl. Python-reference pins. No bugs.
OK cold_prefix: is_cold_prefix (strict-gt, NaN→warm, None-TTL→warm, margin direction documented), TTL authority chain (request ttl > env OFF > env hints > 300 default; never infer off from absence), truthy-set divergence DOCUMENTED ({1,true,yes} vs tool-search gate — relates to FINDING-011), spark strip (Responses text-clear + unsigned-thinking drop with keep-one-block floor, signed/redacted untouched), plaintext-reasoning shapes, cold_recompact composition with fail-open. DRIFT-009 (new, self-documented at 394-399): cold_recompact_messages omits Python's per-block lossless folds + router:excluded:* tags.
OK cross_turn_dedup: prefix-monotonic design (earlier-only matching, absolute turns, keep-earliest, folded-spans-as-None break contiguity), uniform-shift folding with pointer-carried delta, trivial-line + anchor-cap guards, char-count thresholds (multibyte parity comments), py_repr quote parity, protected-as-target-only, frozen/cache_control protection both levels, determinism test-pinned, monotonicity self-test. dedup_messages shares CHECK-017 shape gap (Anthropic tool_result + OpenAI role=tool only — Responses input items not covered).
No bugs in any of the three.

## Entry 252 — 2026-09-14 completion: proxy hot-file gaps (surveyed)
FINDING-025 (websocket_codex.rs:334-335): cache_write_tokens and uncached_tokens store identical `(input_tokens-cached).max(0)` — double-counts if summed downstream.
FINDING-026 (websocket_codex.rs:400-401): bytes_before is outer envelope while tokens_before is inner-only — metrics denominator mismatch.
FINDING-027 (websocket_codex.rs:240): X-Client:codex stamp only on /v1/responses paths; other is_codex_responses_path routes (/v1/codex/responses, /backend-api/...) never stamped.
FINDING-028 (websocket_codex.rs:2055): fallback SSE `strip_prefix("data: ")` misses spec-legal `data:{...}` — events silently dropped.
FINDING-029 (image_compression_decision.rs:27-28): HashMap `.get("x-headroom-bypass")` case-sensitive vs canonical headers.rs:16 HeaderMap path — non-lowercased key misses bypass; decide() has zero non-test callers (live path proxy.rs:6003 uses image_optimize directly).
FINDING-030 (live_zone_anthropic.rs:317-319): whole-body `serde_json::to_vec(&parsed)` re-serialize vs byte-range surgery claim :26-28; stale PR-B2 invariant test :763-764; `let _ = e3_skipped;` :311 discards gate signal.
FINDING-031 (bedrock/eventstream_to_sse.rs:227-228 vs :235): "byte-level scan not full parse" comment vs actual serde_json::from_slice; alloc doc +8 vs extra+8 mismatch :122-123 vs :211-215; Sse/chunk/metadata Emit without `event:` dropped by AnthropicStreamState :196-198.
FINDING-032 (bin/headroom_cli/copilot_auth.rs:49-50): `.split(':').next()` breaks IPv6 `[::1]:8443`; :209/:213 write-then-chmod TOCTOU world-readable window; :69-79 post_json without error_for_status loses server message.
FINDING-033 (bin/offload_replay.rs:53-77): sanitize() collision a/b vs a:b both a_b overwrites corpus; :199 cross_session_seed:false hardcoded vs proxy.rs:541 config; :158-166 silent skip vs :74-75 abort claim.

## Entry 253 — 2026-09-14 completion: cache_stabilization + observability (surveyed)
OK tool_prune, message_breakpoints, ephemeral_spans, tool_order, billing_header, ttl_order, role_sentence, beta_sticky (nit :185 verbatim-vs-normalized only).
FINDING-034 (anthropic_cache_control.rs:173-184): under-scans vs ttl_order.rs:177-182 + cache_ttl.rs:249-250 — misses message-level + nested tool_result markers, risks double-marker; doc order drift :49 vs ttl_order.rs:3-4.
FINDING-035 (capture.rs:38-46): run_id() as_secs + SEQ reset → same-second restart overwrites corpus; :80,:120 unbounded thread::spawn per request.
FINDING-036 (cache_ttl.rs:246-260): pins message+block not nested tool_result.content[] walked at ttl_order.rs:214-218; :325-346 client_ttl_shape recurses into input_schema vs anthropic_cache_control.rs:160 avoidance; :220 is_anchor_turn anchors on 0/1-message bodies.
MINOR-074 (working_dir.rs:226): `rest.find(char::is_whitespace)` truncates `/home/my dir` → corrupts on hold.
FINDING-037 (observability redact_metrics.rs:17-30 et al.): OnceLock<IntCounter> binds first registry, later *_get(other) reads global — breaks custom-registry isolation (same replay_alternates, tail_breakpoint:39-44, ctx_metrics:14-27).
FINDING-038 (upstream_health.rs:181-190): rejections_total counts any non-2xx incl. 429/5xx while window pushes proxy-faults only — dashboard overcounts vs module doc :16-21.

## Entry 254 — 2026-09-14 completion: core signals + transforms gaps (surveyed)
FINDING-039 (signals/keyword_detector.rs:163-173): doc "highest-priority" vs code earliest-position — early todo + later error returns Importance not Error; fix: scan all, return max priority_for. line_importance/mod/tiered OK.
FINDING-040 (transforms/pipeline/reformats/log_template.rs:193-195): wildcard-always-match inflates sim, over-absorbs equal-token-count lines; :296-298 split_whitespace + :260-267 single-space rejoin is byte-lossy vs "lossless" claim. json_minifier OK (note: applies_to JsonArray-only per detector fold).
FINDING-041 (transforms/pipeline/offloads/search_offload.rs:169-181): hyphen-digit `file-2-backup.py:42:` matches `-2-` early → returns "file", deflates clustering. diff_offload docs-only drift :260-262 (wrapper-must-store comment stale); log_offload OK; json_offload OK; diff_noise OK (terminology hunks-vs-segments only).
FINDING-042 (transforms/pipeline/offloads/prose_field.rs:90): `store.put` unchecked vs siblings warn — failure emits unresolvable marker.
FINDING-043 (smart_crusher/compaction/formatter.rs:240,450): per-bucket `[N]` never shows drops (original_count=rows.len()); JSON SIZE exact bytes vs CSV/KV humanize_bytes — contract :32-34 must define units.

## Entry 255 — 2026-09-14 completion: contrib/docs/parity/simulators/top-level (surveyed)
FINDING-044 (.gitleaks.toml:11,17-18,23-24): sbom/benchmarks allowlist dead (dirs absent); comments cite headroom/config.py, headroom/cli/proxy.py (not at root post-move). Regexes narrow — no over-broadening.
OK parity_run (fixtures path exists; --only unknown exits 0 silently) + diff_fixture (BTreeSet union correct).
OK simulators presentation (413 cap, invalid headers warn-skip) + simulator_http (209 passthrough intentional).
FINDING-045 (statusline-cache-perf.sh:129-138,220-221): uncached_share computed then dropped from output — described signal missing.
FINDING-046 (review-gate.sh:53): `!\[0-9]+` never matches `!554` (should be `![0-9]+`); :57 + ticket-gate.sh:14 hardcode $HOME/headroom vs $HEADROOM_REPO; issue-object overlap review-gate:331-333 vs ticket-gate:155-156.
FINDING-047 (spark-poster ticket_file.py:87 vs :70): TURNS_JQ `.[-10:]` vs TURNS=10 duplicated; :182-192 BIN_PLACEHOLDER-first order rewrites literal mentions; spark-goahead.sh:50-62 legacy vs review-gate:160-180 canonical; thread_dossier.py:135 int() unguarded; install-credential.sh:50 GNU sed -i breaks macOS.
FINDING-048 (docs app/llms.mdx/docs/[[...slug]]/route.ts:8,18-23): generateStaticParams {lang,slug} vs [[...slug]]-only route — lang extraneous; sitemap.ts:15 lastModified:now churns cache; robots.ts:40 host: deprecated. proxy/search/llms.txt routes OK.

## Entry 256 — 2026-09-14 completion: final sweep drift (flags/docs/Makefile/CI)
DRIFT-010 (flags count): AGENTS.md:85 "124 options" stale — binary exposes 131 unique long flags (133 with -h/-V); docs/flags.md matches binary exactly (fresh, regenerate recipe :1-21 followed). contrib/headroom-flags.sh sets 84 active + 19 commented — matches README:139 "about 85"; 47 binary flags absent by design (listen/upstream/ctx/foundry/vertex/bedrock defaults).
DRIFT-011 (Makefile vs CI): Makefile:154-158 == rust.yml:141-146 (no drift, "mirrored" holds). Commitlint in Makefile:150 has zero .github/ enforcement — local-only. CI-only simulator-e2e/wheels/audit by design; parity matches (rust.yml:267-268 == Makefile:49-50); audit job references missing audit.toml (FINDING-008 open); build-e2e-wrap known-broken (DRIFT-001).
DRIFT-012 (docs vs code): README:207-209 + AGENTS:141-143 tree (headroom/,tests/,sdk/,plugins/ at root) stale — mirror is upstream-python/ (wiki/ARCHITECTURE:358-363 correct). README:112-113 + AGENTS:113-114 "all stabilizers off" vs --help defaults true (--cache-stable-tool-order, --cache-tail-breakpoint, --ctx-drop-prior-thinking, --enable-bedrock-native). Layout table + workspace map + cited files (sidecar.rs:50, config.rs:25, live_zone, cache_stabilization/20 files, paths.rs) verified OK.

## Entry 257 — 2026-09-14 completion: test evidence + close (prior scope)
`cargo test -p headroom-core --lib`: 2173 passed, 0 failed, 2 ignored (1.66s).
`cargo test -p headroom-proxy --lib --quiet`: 2318 passed, 0 failed, 1 ignored (30.92s).
Combined 4491 green — matches 2026-09-11 Entry 193, corroborates audit: no live-path logic bugs in verified modules; pinned-buggy-behavior tests pass as documented.
Numbering note: Friday had already reached Entry 251 (middle of file); 2026-09-14 survey entries numbered 252-256 to avoid collision (196-200 duplicates resolved).
Coverage: 515 in-scope files; hot paths deep, ports/fixtures/ops survey; checklist ticked. EXCLUDED per scope: docs/notes working notes (stale, not spec), upstream-python/ read-only mirror; e2e/ absent; tests/ executed not line-read. No code changes — audit file only.

## Entry 258 — 2026-09-14 fix pass, batch 1 (trust-but-verify dispositions)
FIXED FINDING-016 (kompress.rs: stride=max(1), zero chunk_words no longer hangs).
FIXED MINOR-057 (ctx/inject.rs: poison recovery via into_inner, matches repo norm).
FIXED MINOR-041 (compression_batches.rs: empty batch early-passthrough, no [0] panic).
FIXED FINDING-025 (websocket_codex.rs: outcome now sets cache_inferred=true — write leg was inferred, billing double-counted cached+2xuncached).
FIXED FINDING-028 (websocket_codex.rs:2055 fallback accepts `data:` with/without space, matches 4 other SSE parsers).
FIXED FINDING-024 (openai/request.rs translate_user_message: images → image_url parts, others → placeholder; image-only message no longer vanishes). +2 tests.
FIXED FINDING-022-partial (ctx/identity.rs: conversation_key + message_count fall back to Responses `input[0]`/`input` — distinct conversations no longer merge, turn_n no longer 0). +1 test. Full Responses extraction/classification still open (feature-sized).
FIXED FINDING-020 (memory on Responses: flat responses_tools() defs injected for OpenAiResponses; get_tool_name/id/input accept flat function_call fields with call_id precedence; results reshaped to function_call_output in continuation). +2 tests.
FIXED MINOR-070 (stage_timer.rs: lifetime-bound guard, raw pointer + unsafe gone; zero callers affected).
FIXED MINOR-055 (background_compression.rs: lazy drain spawn on first enqueue, graceful drop outside runtime). +1 test.
NOT-FIXED FINDING-027 (stamp scope): verified test-pinned (websocket_codex.rs:2249-2252) and a faithful Python port (auth_policy.py CODEX_RESPONSES_PATH=/v1/responses) — needs design-intent ruling, not a silent change.
PARTIAL FINDING-017 (exclude-tools): docs fixed (config.rs caveat: Anthropic-only); behavior change deferred — needs cross-message id→name resolution in OpenAI/Responses planners, fails-unsafe if rushed.
Tests after batch 1: core --lib 2175 passed, proxy --lib 2324 passed (+6 new), 0 failed. Clippy -D warnings still reports 42 pre-existing lints in untouched regions (verified outside fix hunks).

## Entry 259 — 2026-09-14 fix pass, batch 2 (trust-but-verify dispositions)
FIXED FINDING-038 (upstream_health.rs: doc-only — counter is raw per-status denominator material, alert window is proxy-faults-only; HELP text confirms, added 9-line comment so the split reads as intentional).
FIXED FINDING-039 (keyword_detector.rs: first_word_match now scans all matches, returns max priority_for; early todo no longer shadows later error). +1 test.
FIXED FINDING-040 (log_template.rs: doc-only — wildcard-always-match + split_whitespace normalization now load-bearing documented; "lossless" narrowed to token-level).
FIXED FINDING-041 (search_offload.rs: colon-first two-pass extract_file_prefix; `file-2-backup.py:42:` clusters under full filename). +1 test.
FIXED FINDING-042 (prose_field.rs: store.put failure now warns like siblings; marker parity).
FIXED FINDING-043 (formatter.rs: doc-only — SIZE units differ by contract (JSON exact bytes vs CSV/KV humanize_bytes), [N] is kept-rows; also fixed stale `__total:N` doc on include_drop_summary; compactor never drops rows today so [N]==total, __dropped path is for a future budget).
FIXED FINDING-044 (.gitleaks.toml: dropped dead `sbom/.*`, fixed stale `headroom/...` prefixes to `upstream-python/headroom/...`; toml parses).
FIXED FINDING-045 (statusline-cache-perf.sh: uncached share now surfaced in output with non-numeric guard).
FIXED FINDING-046-partial (review-gate.sh: `![0-9]+` regex fix; HEADROOM_REPO fallback in review-gate + ticket-gate; overlap review:331-333 vs ticket:155-156 documented as intentional non-identical mirrors). REVERTED the in-tree BARE-yes rewrite (transcript-question check) — out of scope for the finding, higher false-divert risk; restored HEAD version verbatim (bash -n clean, live symlink so installed hook picks it up).
FIXED FINDING-047-partial (thread_dossier.py: int(line) guarded — malformed anchor keeps commits, skips snippet; install-credential.sh: macOS `sed -i ''` branch). NOT-FIXED: TURNS_JQ `.[-10:]` vs TURNS=10 duplication (cosmetic, single site, jq is the authority), BIN_PLACEHOLDER-first replace order (correct — config placeholders resolved before turns are substituted, literal mentions in turns untouched), spark-goahead.sh legacy (retired per spark-poster/README.md:24, script stays for manual use).
FIXED FINDING-048 (docs: dropped extraneous `lang` from llms.mdx [[...slug]] + og [...slug] generateStaticParams — no i18n in source.config.ts; sitemap no longer stamps `new Date()` on all URLs; robots dropped deprecated `host`).
FIXED DRIFT-010 (AGENTS.md: "124 options" → 131 (133 with -h/-V), verified against binary).
FIXED DRIFT-012-partial (AGENTS.md + README.md: Python-tree path → `upstream-python/`; "all stabilizers off" → several-ship-on with named examples; README stabilizer table gains a Default column with verified per-row values). configuration.mdx:112 "off by default" LEFT AS IS — verified in-scope (cold-prefix/reasoning section is Python-proxy-only; Rust wires only HEADROOM_COLD_RECOMPACT). cache-optimization.mdx:89 "both off" LEFT AS IS — refers to cold-prefix hook + reasoning Kompress, both genuinely off.
FIXED DRIFT-011/FINDING-008 (rust.yml audit comment: audit.toml never existed for cargo-audit; directs to deny.toml [advisories] instead; yaml parses).
INCIDENTAL (pre-existing, found via fmt gate): integration_local_model.rs had a corrupted test fn name (`async fn [headroom: unresolved ...]`, unclosed delimiter — fmt + all integration builds broken) plus doubled `#[tokio::test]` and `auth:`→`auth_env:` rename fallout plus exact-vs-prefix route mismatch (`prefix_match:false` with model `claude-zen-hold-probe` → 404). Repaired: proper fn name, single attribute, auth_env, prefix_match:true. File now 20/20 green.
Tests after batch 2: core --lib 2180 passed, proxy --lib 2353 passed, workspace 5123 passed / 13 ignored (113 suites), 0 failed. `cargo fmt --check` clean, `cargo clippy --workspace -- -D warnings` 0 errors.


























































































## Entry 260 — 2026-09-14 doc-only fix pass, batch 3 (MINOR batch 3)
FIXED MINOR-018 (content_detector.rs dispatch-order doc: added missing Tabular ≥0.6 and StructuredConfig ≥0.6 steps with ordering notes; doc now matches code order JSON → diff → HTML → search → log → tabular → config → code → text).
FIXED MINOR-022 (read_maturation.rs:43: `enabled` comment said enabled-by-default, `Default` says false — comment now reads disabled-by-default).
FIXED MINOR-062 (ranker.rs parse_timestamp: docstring claimed ISO-8601, body returns None for non-numeric — doc trimmed to epoch-seconds-only).
FIXED MINOR-064 (prefix_replay.rs:3484: deleted stray test-TTL comment above `history_will_be_rewritten`; it described a different fn).
CHECKED FINDING-014 (diff_compressor.rs ~600-604 vs ~690/797-805: doc claims `parse_warnings` entry for >4-parent octopus headers, code pushes it — doc matches, left alone).
FIXED MINOR-005 (rollout.rs `is_enabled(feature, _explicit)`: one-line doc noting `_explicit` reserved/ignored; no rename).
CHECKED DRIFT-007: already documented in-test (content_detector.rs `json_objects_are_never_config` NOTE names the single-JSON-object Rust-vs-Python divergence as pre-existing and deliberately unasserted) — left alone.
Dispositions recorded: DRIFT-007 REAL-DOC-ONLY; FINDING-007/010 REAL-NEEDS-FIX (other agent fixing); FINDING-014 REAL-DOC-ONLY; FINDING-015/029 REAL-WONTFIX; FINDING-023/032/033/035 REAL-NEEDS-FIX already in working tree; FINDING-026/030/031/034/036 RULED-NOT-A-BUG (stale-line reads, code already correct); FINDING-037 REAL-DOC-ONLY; MINOR-005/018/022/062/064 REAL-DOC-ONLY (this agent); MINOR-006/007/009/014/015/017/021/027/028/029/030/031/032/042/046/047/054/060/061/068 REAL-WONTFIX accepted; MINOR-063/066/067/071/072 REAL-NEEDS-FIX (other agent fixing); MINOR-065/074 NOT-REAL; MINOR-016 REAL-NEEDS-FIX already in tree.
No tests run (per instruction). No commit (per instruction).

## Entry 261 — 2026-09-14 double-check pass (3 subagents, worktree incl. Entries 258-260 fixes)
DOUBLE-CHECKED via fresh reads (no edits by checkers):
- REFUTED-fixed already in tree: FINDING-007 (restart log line :109-110), FINDING-010 (base.rs (all,[]) + tests), FINDING-023 main (mapped+v4 arms+tests; residual 0.0.0.0/8 nit below), FINDING-032 all three (bracket-IPv6, mode-0600 create, error_for_status), FINDING-033 all three (hash-suffixed sanitize, env-driven cross_session_seed, skip-counted contract), FINDING-035a (nanos+pid run_id), MINOR-016 (empty early-passthrough), MINOR-063 (warn on malformed), MINOR-066 (NamedTempFile persist), MINOR-067 (warn per bad cert, skip-bad retained), FINDING-013 (empty sentinel), FINDING-018 (protect_tool_results force-merged into exclude_tools, config.rs:2817-2843).
- CONFIRMED still open: FINDING-009 (delete_user edges), FINDING-023 residual (0.1.2.3 passes is_unspecified), FINDING-035b (thread::spawn per request, documented intentional), MINOR-071 (512 vs 200 norm), MINOR-072 partial (marker-gated but no PAYG gate + double sort), FINDING-012 (latest_message None dead branch), FINDING-017 (Anthropic-only, documented), FINDING-019 (lifecycle/maturation unwired, NO-OP-labeled).
- NOT-FIXED by design: FINDING-027 (test-pinned + Python-port faithful, needs intent ruling).

## Entry 262 — 2026-09-14 residual fix pass (this agent)
FIXED FINDING-009 (memory_records.rs delete_user: tx + edge delete before records + regression test delete_user_clears_entity_edges).
FIXED FINDING-023 residual (fetch.rs check_ip: octets[0]==0 rejected alongside is_unspecified + 0.1.2.3 test).
FIXED MINOR-071 (response_arms.rs both local_model_upstream_error sites: 512→200 chars, body_len retained; matches 200-char repo norm).
FIXED MINOR-072 (batch_anthropic.rs compress_batch_item outer sort: PAYG-gated + marker-gated, double sort removed).
FIXED FINDING-012 (live_zone.rs Responses dispatcher: track latest user-role `message` per PR-C3 spec; assistant messages excluded; renamed message_user_content_not_in_live_zone → message_user_tiny_content_below_threshold asserting BelowByteThreshold NoChange + latest index).
LEFT AS IS: FINDING-017 behavior (fails-unsafe cross-message id→name work, documented Anthropic-only), FINDING-019 wiring (feature-sized, NO-OP-labeled), FINDING-027 (needs ruling), FINDING-035b spawn (diagnostic-only, documented).
Tests: core --lib 2181 passed, proxy --lib 2349 passed, live_zone 65 passed; `cargo fmt --check` clean; clippy -D warnings shows only pre-existing lints (none in edited files).

## Entry 263 — 2026-09-15 Zen reasoning-strip fix (live incident, not a numbered finding)
FIXED strip_unreplayable_reasoning (routed/quirks.rs) dropping the entire reasoning item on OpenCode Zen once exit rotation invalidated `encrypted_content`, discarding the visible chain of thought along with the unreplayable blob (see docs/notes/learnings/zen-reasoning-blob-vs-exit-rotation.md). Now strips only `id` + `encrypted_content`, keeps `summary`; openai/request.rs's reasoning-item call site threads the visible `thinking` text through via `reasoning_input_item_with_summary` (already added, unwired, in the committed tree) so there is a summary to keep. Surfaced live: two opencode sessions (ses_f5ed10b74ffe9LATpJD0REuhO1, ses_f5ed8acfbffe8QIMaFG8EYIx5T) died on the documented 400 mid-task; this closes that session's pending "Audit reasoning strip on Zen route (minimum-necessary)" todo item. Updated the 3 tests that asserted the old drop-everything behavior (routed::quirks::zen_strips_the_reasoning_replay, routed::translation::zen_route_sends_no_encrypted_reasoning, openai::stream::reasoning_envelope_round_trips_through_the_client).
Tests: proxy --lib full suite 2825 passed, 9 ignored. `rustfmt` scoped to the 4 touched files, no changes needed. Commit 2e2c31b7.

## Entry 264 — 2026-09-15 Spark request-volume verification (this agent)
CHECKED "Verify Spark request volumes (retries/sidecar/loops)" (dead opencode session's pending todo). Traced the three volume-bounding mechanisms on the Zen/muse-spark route: acquire_zen_slot (routed/upstream_gate.rs, global in-flight cap, default 4) already has solid unit coverage (slot_immediately_when_free, slot_fail_open_without_holding, slot_waits_for_release, zero_max_disables_without_touching_counter) — the 2026-09-14 incident it exists for (40 parallel Zen 429s in an hour, a 7-wide subagent burst) is covered. hold_for_rotation (routed/zen_hold.rs) had a real gap: 0 unit tests, and the one integration test that looked like it covered the hold (zen_hold_recovers_after_fast_budget_spent) explicitly can't — its own comment says wiremock never classifies as the Zen host, so it exercises the plain fast-retry loop, not the hold. retry.rs has two unit tests that force is_zen to reach the real hold (zen_hold_recovers_flapping_429, zen_hold_ignores_retry_after_past_the_cap), but both are recovery-eventually cases; nothing asserted the never-recovers path terminates and stays bounded.
FIXED the gap: added zen_hold_gives_up_after_budget_when_never_recovering (routed/retry.rs) — always-429 mock, tiny budget/backoff, asserts the hold returns the still-429 response within a bounded wall-clock time and a bounded hit count rather than hanging or spamming. Ran 5x to rule out timing flakiness.
No other loop or sidecar-duplication risk found: the hold sits before any byte is forwarded (per zen_hold.rs's own module doc), so a re-send on 429 duplicates nothing already streamed to the client.
Tests: proxy --lib full suite 2826 passed, 9 ignored (2825 + the 1 new test). `rustfmt` scoped to the touched file. Commit d87c2c32.
