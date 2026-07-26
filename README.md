<div align="center"><pre>
  ██╗  ██╗███████╗ █████╗ ██████╗ ██████╗  ██████╗  ██████╗ ███╗   ███╗
  ██║  ██║██╔════╝██╔══██╗██╔══██╗██╔══██╗██╔═══██╗██╔═══██╗████╗ ████║
  ███████║█████╗  ███████║██║  ██║██████╔╝██║   ██║██║   ██║██╔████╔██║
  ██╔══██║██╔══╝  ██╔══██║██║  ██║██╔══██╗██║   ██║██║   ██║██║╚██╔╝██║
  ██║  ██║███████╗██║  ██║██████╔╝██║  ██║╚██████╔╝╚██████╔╝██║ ╚═╝ ██║
  ╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝╚═════╝ ╚═╝  ╚═╝ ╚═════╝  ╚═════╝ ╚═╝     ╚═╝
              The context compression layer for AI agents
</pre></div>

<p align="center"><strong>60–95% fewer tokens (for JSON data), 15-20% fewer tokens (for coding agents) · library · proxy · MCP · content-aware compressors · local-first · reversible</strong></p>

<p align="center">
  <a href="https://github.com/chopratejas/headroom/actions/workflows/ci.yml"><img src="https://github.com/chopratejas/headroom/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://app.codecov.io/gh/chopratejas/headroom"><img src="https://codecov.io/gh/chopratejas/headroom/graph/badge.svg" alt="codecov"></a>
  <a href="https://pypi.org/project/headroom-ai/"><img src="https://img.shields.io/pypi/v/headroom-ai.svg" alt="PyPI"></a>
  <a href="https://www.npmjs.com/package/headroom-ai"><img src="https://img.shields.io/npm/v/headroom-ai.svg" alt="npm"></a>
  <a href="https://huggingface.co/chopratejas/kompress-v2-base"><img src="https://img.shields.io/badge/model-Kompress--v2--base-yellow.svg" alt="Model: Kompress-v2-base"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-blue.svg" alt="License: Apache 2.0"></a>
  <a href="https://headroom-docs.vercel.app/docs"><img src="https://img.shields.io/badge/docs-online-blue.svg" alt="Docs"></a>
</p>

<!-- mcp-name: io.github.headroomlabs-ai/headroom -->

<p align="center">
  <a href="https://headroom-docs.vercel.app/docs">Docs</a> ·
  <a href="#get-started-60-seconds">Install</a> ·
  <a href="#proof">Proof</a> ·
  <a href="#agent-compatibility-matrix">Agents</a> ·
  <a href="https://discord.gg/yRmaUNpsPJ">Discord</a> ·
  <a href="llms.txt">llms.txt</a>
</p>

<p align="center"><sub>
  <b>AI agents / LLMs:</b> read <a href="llms.txt"><code>/llms.txt</code></a> here, or fetch <a href="https://headroom-docs.vercel.app/llms.txt">the live index</a> / <a href="https://headroom-docs.vercel.app/llms-full.txt">full docs blob</a>.
</sub></p>

---
<p align="center"><a href="https://trendshift.io/repositories/20881" target="_blank"><img src="https://trendshift.io/api/badge/repositories/20881" alt="chopratejas%2Fheadroom | Trendshift" style="width: 250px; height: 55px;" width="250" height="55"/></a></p>

Headroom compresses everything your AI agent reads — tool outputs, logs, RAG chunks, files, and conversation history — before it reaches the LLM. Same answers, fraction of the tokens.

<p align="center">
  <img src="HeadroomDemo-Fast.gif" alt="Headroom in action" width="820">
  <br/><sub>Live: 10,144 → 1,260 tokens — same FATAL found.</sub>
</p>

## What it does

- **Library** — `compress(messages)` in Python or TypeScript, inline in any app
- **Proxy** — `headroom proxy --port 8787`, zero code changes, any language
- **Agent wrap** — `headroom wrap claude|codex|grok|copilot|cursor|aider|opencode|cline|continue|goose|openhands|openclaw|vibe|omp|zcode` in one command; undo with `headroom unwrap <tool>`
- **MCP server** — `headroom_compress`, `headroom_retrieve`, `headroom_stats` for any MCP client
- **Cross-agent memory** — shared store across Claude, Codex, Gemini, Grok, auto-dedup
- **`headroom learn`** — mines failed sessions, writes corrections to `CLAUDE.local.md` (default, gitignored) or `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` / `GROK.md`
- **Output token reduction** — trims what the model *writes back* (not just what you send): drops ceremony/restated code and skips deep "thinking" on routine steps. See [Output token reduction](#output-token-reduction-cut-what-the-model-writes-back).
- **Reversible (CCR)** — originals are cached for retrieval on demand

## How it works (30 seconds)

```
 Your agent / app
   (Claude Code, Cursor, Codex, LangChain, Agno, Strands, your own code…)
        │   prompts · tool outputs · logs · RAG results · files
        ▼
    ┌────────────────────────────────────────────────────┐
    │  Headroom   (runs locally — your data stays here)  │
    │  ────────────────────────────────────────────────  │
    │  CacheAligner  →  ContentRouter  →  CCR            │
    │                    ├─ SmartCrusher   (JSON)        │
    │                    ├─ CodeCompressor (AST)         │
    │                    └─ Kompress-v2-base (text, HF)  │
    │                                                    │
    │  Cross-agent memory  ·  headroom learn  ·  MCP     │
    └────────────────────────────────────────────────────┘
        │   compressed prompt  +  retrieval tool
        ▼
 LLM provider  (Anthropic · OpenAI · Bedrock · …)
```

- **ContentRouter** — detects content type, selects the right compressor
- **SmartCrusher / CodeCompressor / Kompress-v2-base** — compress JSON, AST, or prose
- **CacheAligner** — stabilizes prefixes so provider KV caches actually hit
- **CCR** — stores originals locally; LLM calls `headroom_retrieve` if it needs them

→ [Architecture](https://headroom-docs.vercel.app/docs/architecture) · [CCR reversible compression](https://headroom-docs.vercel.app/docs/ccr) · [Kompress-v2-base model card](https://huggingface.co/chopratejas/kompress-v2-base)

## Get started (60 seconds)

```bash
# 1 — Install
uv tool install --python 3.13 "headroom-ai[all]"  # CLI as a global tool in a self-contained virtual env
pip install "headroom-ai[all]"                    # Python — ships the `headroom` CLI
npm install headroom-ai                           # TypeScript SDK only — no `headroom` CLI

# 2 — Pick your mode  (the `headroom` commands below come from the uv or pip install)
headroom deploy                         # turnkey local deployment + agent config
headroom wrap claude                    # wrap a coding agent
headroom proxy --port 8787              # drop-in proxy, zero code changes
# or: from headroom import compress      # inline library

# 3 — Verify setup and see the savings
headroom doctor                         # health check — confirms routing is working
headroom perf
headroom dashboard                      # live savings dashboard (proxy must be running)
```

To use headroom, it is recommended you launch a wrapped agent session each time so that all necessary setup is completed. When wrapping a coding agent, headroom starts a local proxy, installs **Serena** for semantic code navigation, and launches a coding agent session configured to proxy requests through headroom.

The `headroom` CLI ships **only** via the PyPI package. The npm `headroom-ai` is the TypeScript SDK — a library you import (`import { compress } from 'headroom-ai'`), not a CLI, so it provides no `headroom` command.

Granular extras: `[proxy]`, `[mcp]`, `[ml]`, `[code]`, `[memory]`, `[vector]` (optional HNSW backend — needs a C++ toolchain, not in `[all]`), `[relevance]`, `[image]`, `[agno]`, `[langchain]`, `[evals]`, `[pytorch-mps]` (Apple-GPU memory-embedder offload — set `HEADROOM_EMBEDDER_RUNTIME=pytorch_mps`). Requires **Python 3.10+**.

### Codex / global install

If Codex or another MCP client cannot inherit a shell `PATH` reliably, install Headroom as a persistent uv tool and point the client at the absolute binary path:

```bash
uv tool install "headroom-ai[all]"
command -v headroom
```

Then use the returned path in MCP config:

```toml
[mcp_servers.headroom]
command = "/absolute/path/from/command-v/headroom"
args = ["mcp", "serve"]
```

`command = "headroom"` only works when the client starts with a `PATH` that already includes the uv tool directory.

## Proof

**Savings on real agent workloads:**

| Workload                      | Before | After  | Savings |
|-------------------------------|-------:|-------:|--------:|
| Code search (100 results)     | 17,765 |  1,408 | **92%** |
| SRE incident debugging        | 65,694 |  5,118 | **92%** |
| GitHub issue triage           | 54,174 | 14,761 | **73%** |
| Codebase exploration          | 78,502 | 41,254 | **47%** |

**Accuracy preserved on standard benchmarks:**

| Benchmark  | Category | N   | Baseline | Headroom | Delta      |
|------------|----------|----:|---------:|---------:|------------|
| GSM8K      | Math     | 100 |    0.870 |    0.870 | **±0.000** |
| TruthfulQA | Factual  | 100 |    0.530 |    0.560 | **+0.030** |
| SQuAD v2   | QA       | 100 |        — |  **97%** | 19% compression |
| BFCL       | Tools    | 100 |        — |  **97%** | 32% compression |

Reproduce: `python -m headroom.evals suite --tier 1` · [Full benchmarks & methodology](https://headroom-docs.vercel.app/docs/benchmarks)

## Output token reduction (cut what the model writes back)

Everything above shrinks the prompt you **send**. But you also pay for every
token the model **writes back** — and on Opus-class models output costs 5× input.
A lot of that output is waste: "Great, let me…" preambles, re-printing code you
just showed it, and deep "thinking" on routine steps like reading a file.

Headroom can trim that too, from the proxy, without you changing any code:

- **Verbosity steering** — appends a short "be terse, don't restate context"
  note to the end of the system prompt (so your prompt cache still hits).
- **Effort routing** — when a turn is just the model resuming after a tool result
  (a file read, a passing test), it dials the model's thinking effort down. New
  questions and errors keep full effort.

Applies to Anthropic `/v1/messages` **and** OpenAI-compatible endpoints
(`/v1/chat/completions`, `/v1/responses`). Effort routing uses
`reasoning_effort` on OpenAI, `thinking.budget_tokens` /
`output_config.effort` on Anthropic — same clamp-only invariant on both
paths, same `output_shaper:*` label vocabulary.

Turn it on:

```bash
export HEADROOM_OUTPUT_SHAPER=1     # off by default
headroom proxy --port 8787
```

> **Already running a proxy?** These switches are read *live* on every request,
> so a proxy that `headroom wrap` **reused** (rather than started) would not see
> a value you export afterwards — its environment was snapshotted at launch.
> `headroom wrap` now hot-syncs your current settings to the running proxy via a
> loopback `POST /admin/runtime-env`, so they take effect immediately with **no
> restart** (no cold start, no dropped requests, no lost caches). Set them before
> you `wrap`. On a shared proxy these overrides are global — the last explicit
> setting wins.

**Learn the right terseness for you.** People don't *say* how terse they want
answers — they *show* it (they interrupt long replies, or move on before they
could have read them). `headroom learn --verbosity` reads your past sessions and
picks the level automatically:

```bash
headroom learn --verbosity            # preview what it found (dry run)
headroom learn --verbosity --apply    # save it; the proxy uses it from now on
```

**See how many output tokens you saved.** Output savings are *counterfactual* —
we never see what the model *would* have written — so Headroom reports an honest
**estimate with a confidence range**, never a made-up number:

```bash
headroom output-savings
# Reduction: 31.7%  (95% CI 27.7% … 35.7%)   [estimated]
```

Want a *measured* number instead of an estimate? Leave 10% of conversations
unshaped as a control group: `export HEADROOM_OUTPUT_HOLDOUT=0.1`. The dashboard
shows an **Output Tokens Saved** card next to input compression, labelled
`measured` or `estimated` with the confidence band.

→ Full write-up incl. the measurement methodology: [Output token reduction](https://headroom-docs.vercel.app/docs/savings)

<a href="https://www.star-history.com/?repos=chopratejas%2Fheadroom&type=date&legend=top-left">
 <picture>
   <img alt="Star History Chart" src="https://api.star-history.com/chart?repos=chopratejas/headroom&type=date&legend=top-left" />
 </picture>
</a>

## Agent compatibility matrix

| Agent        | `headroom wrap` | Notes                            |
|--------------|:---------------:|----------------------------------|
| Claude Code  | ✅              | `--memory` · `--code-graph` · `--1m` · `--tool-search` |
| Codex        | ✅              | shares memory with Claude        |
| Grok CLI     | ✅              | routes via `GROK_MODELS_BASE_URL` |
| Cursor       | Manual setup    | starts proxy and prints base URLs for Cursor settings |
| Aider        | ✅              | starts proxy + launches          |
| Copilot CLI  | ✅              | starts proxy + launches          |
| OpenClaw     | ✅              | installs as ContextEngine plugin |
| OpenCode     | ✅              | injects config · starts proxy + launches |
| Cline        | ✅              | starts proxy + injects config    |
| Continue     | ✅              | starts proxy + injects config    |
| Goose        | ✅              | starts proxy + launches          |
| OpenHands    | ✅              | starts proxy + launches          |
| Mistral Vibe | ✅              | starts proxy + launches          |
| Oh My Pi     | ✅              | injects config · starts proxy + launches |
| Cortex Code  | Library only    | 60–65% savings (library mode; no `wrap`) |
| Kimi CLI     | ✅              | OAuth bearer forwarded — log in once |
| ZCode        | ✅              | starts proxy and prints base URLs for ZCode settings |

Any OpenAI-compatible client works via `headroom proxy`. MCP-native: `headroom mcp install`.
Undo durable wrapping with `headroom unwrap <tool>` (supports: `claude`, `copilot`, `codex`, `grok`, `kimi`, `omp`, `opencode`, `openclaw`, `zcode`).
Registry authors can use the canonical [`server.json`](server.json) in the repo root instead of reconstructing the `headroom mcp serve` contract from prose.

### GitHub Copilot CLI subscription mode

Headroom can route GitHub Copilot CLI subscription traffic through the local proxy:

```bash
headroom copilot-auth login
headroom wrap copilot --subscription -- --model gpt-4o
```

This lets Headroom intercept OpenAI-compatible Copilot CLI requests and apply the same proxy compression pipeline before forwarding to GitHub Copilot's hosted API. The wrapper exchanges Headroom's reusable GitHub OAuth token for Copilot's short-lived API token and prints the upstream endpoint as `COPILOT_PROVIDER_API_URL=...` during launch.

`headroom copilot-auth login` stores a Headroom-specific Copilot OAuth token.
This avoids relying on generic GitHub or Copilot CLI tokens that can read
Copilot account metadata but may still be rejected by Copilot's token-exchange
endpoint.

For GitHub Enterprise Server or custom-domain Copilot deployments, set one of
these before launching:

```bash
export GITHUB_COPILOT_ENTERPRISE_DOMAIN=ghe.example.com
# or
export GITHUB_COPILOT_ENTERPRISE_URL=https://ghe.example.com
```

Both variables are supported. If both are set,
`GITHUB_COPILOT_ENTERPRISE_URL` takes precedence.

For GitHub.com Enterprise Cloud URLs such as
`github.com/enterprises/your-enterprise`, do not set an enterprise-domain
override. Headroom uses GitHub's normal token-exchange endpoint and the Copilot
API endpoint advertised for the signed-in account.

Platform support note: macOS auth reuse via Copilot CLI Keychain storage has been smoke-tested. Windows Credential Manager, Linux Secret Service / `secret-tool`, and Docker/CI token-injection paths are implemented or planned as auth-discovery paths, but still need real OS validation before they should be considered fully vetted. For Docker and CI, prefer passing an explicit `GITHUB_COPILOT_TOKEN` or `GITHUB_COPILOT_GITHUB_TOKEN` rather than relying on host keychain access.

## When to use · When to skip

**Great fit if you…**
- run AI coding agents daily and want savings without changing your code
- work across multiple agents and want shared memory
- need reversible compression — originals are retrievable via CCR within the configured TTL

**Skip it if you…**
- only use a single provider's native compaction and don't need cross-agent memory
- work in a sandboxed environment where local processes can't run

<details>
<summary><b>Integrations — drop Headroom into any stack</b></summary>

| Your setup             | Hook in with                                                     |
|------------------------|------------------------------------------------------------------|
| Any Python app         | `compress(messages, model=…)`                                    |
| Any TypeScript app     | `await compress(messages, { model })`                            |
| Anthropic / OpenAI SDK | `withHeadroom(new Anthropic())` · `withHeadroom(new OpenAI())`   |
| Vercel AI SDK          | `wrapLanguageModel({ model, middleware: headroomMiddleware() })` |
| LiteLLM                | `litellm.callbacks = [HeadroomCallback()]`                       |
| LangChain              | `HeadroomChatModel(your_llm)`                                    |
| Agno                   | `HeadroomAgnoModel(your_model)`                                  |
| Strands                | [Strands guide](https://headroom-docs.vercel.app/docs/strands)  |
| ASGI apps              | `app.add_middleware(CompressionMiddleware)`                      |
| Multi-agent            | `SharedContext().put / .get`                                     |
| MCP clients            | `headroom mcp install`                                           |

</details>

<details>
<summary><b>What's inside</b></summary>

- **SmartCrusher** — universal JSON: arrays of dicts, nested objects, mixed types.
- **CodeCompressor** — AST-aware for Python, JS/TS, Go, Rust, Java, C/C++, Perl.
- **Kompress-v2-base** — our HuggingFace model, trained on agentic traces.
- **Image compression** — 40–90% reduction via trained ML router.
- **CacheAligner** — stabilizes prefixes so Anthropic/OpenAI KV caches actually hit.
- **Live-zone compression** — compresses only new bytes (fresh tool output, latest turn); frozen prefix stays byte-identical so provider cache is not busted. History is never dropped.
- **CCR** — reversible compression; LLM retrieves originals on demand.
- **Cross-agent memory** — shared store, agent provenance, auto-dedup.
- **SharedContext** — compressed context passing across multi-agent workflows.
- **`headroom learn`** — plugin-based failure mining for Claude, Codex, Gemini.

</details>

<details>
<summary><b>Pipeline internals</b></summary>

Headroom exposes one stable request lifecycle across `compress()`, the SDK, and the proxy:

`Setup` → `Pre-Start` → `Post-Start` → `Input Received` → `Input Cached` → `Input Routed` → `Input Compressed` → `Input Remembered` → `Pre-Send` → `Post-Send` → `Response Received`

- **Transforms** do the work: CacheAligner → ContentRouter → SmartCrusher / CodeCompressor / Kompress-base (live-zone only; IntelligentContext and RollingWindow were retired in PR-B1).
- **Pipeline extensions** observe or customize lifecycle stages via `on_pipeline_event(...)`.
- **Compression hooks** sit alongside the canonical lifecycle as an additional extension seam.
- **Proxy extensions** remain the server/app integration seam for ASGI middleware, routes, and startup policy.

Provider and tool-specific behavior lives under `headroom/providers/` so core orchestration stays focused on lifecycle, sequencing, and policy.

- **CLI/tool slices**: `headroom/providers/claude`, `copilot`, `codex`, `grok`, `openclaw`
- **Provider runtime slices**: `headroom/providers/claude`, `gemini`, plus shared backend/runtime dispatch in `headroom/providers/registry.py`
- **Core files stay orchestration-first**: `wrap.py`, `client.py`, `cli/proxy.py`, and `proxy/server.py` delegate provider-specific env shaping, API target normalization, backend selection, and transport dispatch.

</details>

## Headroom for teams

Headroom OSS is built for **individual developers**: run `headroom proxy` or `headroom wrap` on your laptop and start cutting tokens in minutes — free, local-first, your data never leaves your machine.

Running it across a **whole engineering org** is a different job: a shared, always-on deployment; centralized config and version rollout; org-wide savings dashboards; SSO and access controls; air-gapped / VPC installs; and someone to call when it matters. That's what we help companies with — self-hosted with support, or fully managed.

**If your team is spending real money on LLM tokens** — Claude Code, Codex, Cursor, or agents running in CI — **and you want those savings across everyone, not just one laptop:**

→ Email **[hello@headroomlabs.ai](mailto:hello@headroomlabs.ai)** with your stack and rough monthly LLM spend, and we'll help you roll Headroom out across your organization.

Everything in this repo stays open source (Apache 2.0). The managed offering is simply for teams that would rather have it deployed, supported, and scaled for them.

## Install

```bash
uv tool install --python 3.13 "headroom-ai[all]"  # CLI, isolated app env
pip install "headroom-ai[all]"                    # Python, everything — includes the `headroom` CLI
npm install headroom-ai                           # TypeScript SDK (library only — no `headroom` CLI)
docker pull ghcr.io/chopratejas/headroom:latest
```

Granular extras: `[proxy]`, `[mcp]`, `[ml]` (Kompress-v2-base), `[code]`, `[memory]`, `[vector]` (optional HNSW backend — needs a C++ toolchain, not in `[all]`), `[relevance]`, `[image]`, `[agno]`, `[langchain]`, `[evals]`, `[pytorch-mps]` (Apple-GPU memory-embedder offload — set `HEADROOM_EMBEDDER_RUNTIME=pytorch_mps`). Requires **Python 3.10+**.

> **Note**: `[all]` covers the core stack but excludes framework adapters. Install them separately: `pip install "headroom-ai[langchain]"` (also `[agno]`, `[strands]`, `[anyllm]`, `[bedrock]`).

Using `uv` for the `headroom` CLI? Prefer `uv tool install` so the command lives in an isolated app environment. On macOS, pass `--python 3.13` if your default `python3` is newer than the current wheel set:

```bash
brew install python@3.13  # if Python 3.13 is not already available
uv tool install --python 3.13 "headroom-ai[all]"
uv tool update-shell      # if ~/.local/bin is not already on PATH
headroom --version
```

For MCP clients such as Codex that do not inherit your interactive shell `PATH`, configure the absolute executable path returned by `command -v headroom`:

```toml
[mcp_servers.headroom]
command = "/Users/you/.local/bin/headroom"
args = ["mcp", "serve"]
```

Current native wheels cover macOS Apple Silicon and Linux. On Intel macOS, use Docker-native install until native wheel support lands.

Using `pipx`? Choose a supported interpreter explicitly:

```bash
pipx install --python python3.13 "headroom-ai[all]"
```

> **Pick 3.13 if you want dollar savings.** The dashboard's *Proxy $ Saved* tile prices compression with [LiteLLM](https://github.com/BerriAI/litellm), and LiteLLM can't be installed on Python 3.14+. On 3.14 token savings still track, but the dollar figure stays `$0.00`. If you already installed on 3.14, switch with `pipx reinstall headroom-ai --python python3.13` and restart the proxy.

→ [Installation guide](https://headroom-docs.vercel.app/docs/installation) — Docker tags, persistent service, PowerShell, devcontainers.

> **CPU requirement (x86/x86_64):** the ONNX-backed features — Magika content
> detection and embedding relevance — use a precompiled ONNX Runtime that needs
> **AVX2**. On x86 hosts without AVX2 (some Docker/QEMU setups and older cloud
> VMs) Headroom automatically falls back to its non-ONNX paths (BM25 relevance,
> heuristic detection) rather than crashing. `arm64`/Apple Silicon needs no AVX2.

### Updating

```bash
headroom update          # detects pip / pipx / uv tool and upgrades in place
headroom update --check  # report the latest release without upgrading
headroom update --pre    # include pre-releases
```

`headroom update` figures out how Headroom was installed (pip/venv, `pip --user`,
pipx, uv tool) and runs the matching upgrade across macOS, Linux, and Windows.
For git checkouts, editable installs, Docker images, and externally-managed
system Pythons (PEP 668) it prints the correct manual step instead of guessing.

The proxy also shows a one-line "update available" notice on startup. It checks
PyPI at most once a day, in the background, and never blocks. Opt out with
`HEADROOM_UPDATE_CHECK=off` (also skipped in `--stateless` mode and CI).

### Corporate / SSL-inspection environments

If `pip install "headroom-ai[all]"` fails with `CERTIFICATE_VERIFY_FAILED`
(`unable to get local issuer certificate`), your network uses **SSL inspection** — a MITM
proxy presenting a company-issued CA. The build backend (`maturin`) downloads `rustup` over a
connection your TLS stack doesn't trust. **Install Rust first** so the build doesn't fetch it:

```bash
# macOS / Linux
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh && rustup default stable
# Windows
winget install Rustlang.Rustup && rustup default stable
```

Restart your shell, then `pip install "headroom-ai[all]"`. A prebuilt wheel avoids the Rust
build entirely where available: `pip install --only-binary headroom-ai headroom-ai`. Prebuilt
wheels are published for Windows (`win_amd64`), Linux (`x86_64` / `aarch64`), and macOS
(Apple Silicon and Intel), so installs on those platforms never need a local Rust toolchain — the
Rust-first dance above is only for the platform-independent sdist fallback when no wheel matches.

Two runtime assets are fetched over TLS; if they are blocked, trust your corporate CA via
`REQUESTS_CA_BUNDLE` / `SSL_CERT_FILE` / `CURL_CA_BUNDLE`:

- **`cdn.pyke.io`** — the ONNX Runtime for the Rust core. Alternatively pre-provide it with
  `ORT_STRATEGY=system` and `ORT_LIB_LOCATION=/path/to/onnxruntime`.
- **`huggingface.co`** — the `kompress-base` compression model. Pre-download it and run with
  `HF_HUB_OFFLINE=1`, or set `HF_ENDPOINT` to a trusted mirror.

Running with compression disabled (pure gateway) requires neither asset.

#### Intel macOS (x86_64-apple-darwin): no prebuilt ONNX Runtime binary (#941)

`ort-sys` ships no prebuilt ONNX Runtime binary for Intel macOS, so a source
build fails by default even outside a corporate-proxy environment. The same
`ORT_STRATEGY=system` mechanism above fixes it — point it at a system ONNX
Runtime instead:

```bash
brew install onnxruntime
ORT_STRATEGY=system \
ORT_LIB_LOCATION="$(brew --prefix onnxruntime)/lib" \
ORT_PREFER_DYNAMIC_LINK=1 \
  pip install "headroom-ai[all]"

# ORT is dlopen'd at runtime too:
export ORT_DYLIB_PATH="$(brew --prefix onnxruntime)/lib/libonnxruntime.dylib"
```

`ORT_LIB_LOCATION` must point at `lib/` (not the bare prefix) and
`ORT_PREFER_DYNAMIC_LINK=1` is required, or `ORT_STRATEGY=system` still
attempts static linking, which the Homebrew keg doesn't provide.

#### "Basic Constraints of CA cert not marked critical" (Python 3.13+ strict mode)

A **different** failure from the one above. If TLS fails with:

```
[SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed:
Basic Constraints of CA cert not marked critical
```

then the corporate CA *is* found and trusted — adding it to a CA bundle changes nothing.
Python 3.13 + OpenSSL 3.x enable `VERIFY_X509_STRICT` by default, which enforces RFC 5280
§4.2.1.9: a CA cert's `basicConstraints` must be marked *critical*. Inspection roots like
Zscaler set `CA:TRUE` without the critical bit, so the chain is rejected.

Set **`HEADROOM_TLS_STRICT=0`** to clear *only* the strict flag from every TLS context
Headroom controls — the proxy's httpx upstream client **and** the urllib3/`huggingface_hub`
path used for model downloads. Chain validation, signature, expiry, and hostname checks all
stay on; this is strictly narrower than disabling verification.

```bash
HEADROOM_TLS_STRICT=0 headroom proxy --port 8787
```

The Rust core's ONNX download (`cdn.pyke.io`) uses a separate TLS stack (rustls / OS trust
store), unaffected by `HEADROOM_TLS_STRICT`. On Windows the corporate root must be in the
**machine** certificate store (browsers already trust it there); or pre-provision ONNX
Runtime with `ORT_STRATEGY=system` + `ORT_LIB_LOCATION=/path/to/onnxruntime` to skip the
download entirely.

## headroom learn

<p align="center">
  <img src="headroom_learn.gif" alt="headroom learn in action" width="720">
</p>

`headroom learn` — mines failed sessions, writes corrections to `CLAUDE.local.md` (default, gitignored; use `--target CLAUDE.md` for the shared team file) / `AGENTS.md` / `GEMINI.md`.

## Documentation

| Start here                                                                    | Go deeper                                                                          |
|-------------------------------------------------------------------------------|------------------------------------------------------------------------------------|
| [Quickstart](https://headroom-docs.vercel.app/docs/quickstart)                | [Architecture](https://headroom-docs.vercel.app/docs/architecture)                 |
| [Proxy](https://headroom-docs.vercel.app/docs/proxy)                          | [How compression works](https://headroom-docs.vercel.app/docs/how-compression-works) |
| [MCP tools](https://headroom-docs.vercel.app/docs/mcp)                        | [CCR — reversible compression](https://headroom-docs.vercel.app/docs/ccr)          |
| [Memory](https://headroom-docs.vercel.app/docs/memory)                        | [Cache optimization](https://headroom-docs.vercel.app/docs/cache-optimization)     |
| [Failure learning](https://headroom-docs.vercel.app/docs/failure-learning)    | [Benchmarks](https://headroom-docs.vercel.app/docs/benchmarks)                    |
| [Configuration](https://headroom-docs.vercel.app/docs/configuration)          | [Limitations](https://headroom-docs.vercel.app/docs/limitations)                  |
| [Persistent installs](https://headroom-docs.vercel.app/docs/persistent-installs) (`headroom init` / `headroom install apply`) | [Savings analytics](https://headroom-docs.vercel.app/docs/savings) (`headroom savings` / `headroom perf` / `headroom doctor`) |

## Compared to

Headroom runs **locally**, covers **every** content type, works with every major framework, and is **reversible**.

|                                                                              | Scope                                          | Deploy                             | Local | Reversible |
|------------------------------------------------------------------------------|------------------------------------------------|------------------------------------|:-----:|:----------:|
| **Headroom**                                                                 | All context — tools, RAG, logs, files, history | Proxy · library · middleware · MCP | Yes   | Yes        |
| [RTK](https://github.com/rtk-ai/rtk)                                        | CLI command outputs                            | CLI wrapper                        | Yes   | No         |
| [lean-ctx](https://github.com/yvgude/lean-ctx)                               | Tool output, files, shell, history             | Proxy · library · middleware · MCP · CLI | Yes | Yes    |
| [Compresr](https://compresr.ai), [Token Co.](https://thetokencompany.ai)    | Text sent to their API                         | Hosted API call                    | No    | No         |
| OpenAI Compaction                                                            | Conversation history                           | Provider-native                    | No    | No         |

> **Stack & integrations.** Headroom is the **proxy** — that's what we build and offer, and it compresses everything flowing through it no matter what sits upstream. Our recommended companion is **[Serena](https://github.com/oraios/serena)** (installed by default when you wrap an agent) for semantic code navigation — plus **Ponytail** if you want leaner model output. Everything else is your call: Headroom vendors the third-party [RTK](https://github.com/rtk-ai/rtk) and [lean-ctx](https://github.com/yvgude/lean-ctx) binaries for shell-output rewriting, but we don't own or control either project — swap between them with `HEADROOM_CONTEXT_TOOL`, or turn them off. You're free to attach your own tooling too — code-memory MCP, Graphify, Caveman, or any MCP server — and Headroom compresses downstream of all of it.

## Contributing

```bash
git clone https://github.com/chopratejas/headroom.git && cd headroom
uv sync --extra dev && uv run pytest
```

Devcontainers in `.devcontainer/` (default + `memory-stack` with Qdrant & Neo4j). See [CONTRIBUTING.md](CONTRIBUTING.md).

## Community

- **[Discord](https://discord.gg/yRmaUNpsPJ)** — questions, feedback, war stories.
- **[Kompress-v2-base on HuggingFace](https://huggingface.co/chopratejas/kompress-v2-base)** — the model behind our text compression.

### Community projects

- **[Claude Code status-line indicator](https://github.com/Ship-Wright/headroom-plugin)** — a Claude Code plugin that shows live Headroom usage in your status line: idle until `headroom_compress` fires, then the running total of tokens saved.

## License

Apache 2.0 — see [LICENSE](LICENSE).
