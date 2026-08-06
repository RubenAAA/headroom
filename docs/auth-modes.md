# Auth modes

Phase F (PR-F1) introduced a per-request **auth-mode classifier** that drives
every downstream compression, cache, and header decision. This page documents
how requests are classified and what each mode changes.

Source of truth: `crates/headroom-core/src/auth_mode.rs` (classifier),
`crates/headroom-core/src/compression_policy.rs` (policy matrix),
`crates/headroom-proxy/src/headers.rs` (header injection gate).

## The three modes

| Mode | Who | Posture |
|---|---|---|
| `payg` | Pay-as-you-go API key (Anthropic `sk-ant-api*`, OpenAI `sk-*`, `x-api-key`, `x-goog-api-key`) | Aggressive: every saved token is money the caller keeps. All compressors, CacheAligner, TOIN learning on. |
| `oauth` | Fixed-cost OAuth / IAM plans (Claude Pro OAuth `sk-ant-oat*`, JWT bearers, Bedrock SigV4, Vertex ADC, any non-Bearer `Authorization` scheme) | Cache-safety first. Per-token cost is opaque to the caller; OAuth scopes pin to `(account, model, session)`, so header/beta drift is the real risk. |
| `subscription` | UX-bound CLIs / IDEs (Claude Code, Codex CLI, Cursor, Copilot, Antigravity — detected by User-Agent) | Stealth. Providers rate-limit by request count and fingerprint programmatic traffic, so Headroom must be invisible on the wire. |

## Classification (`classify(headers) -> AuthMode`)

A pure function over the request headers; most-specific signal wins:

1. **Subscription UA prefix** (`claude-cli/`, `claude-code/`, `codex-cli/`,
   `cursor/`, `claude-vscode/`, `github-copilot/`, `anthropic-cli/`,
   `antigravity/`, matched case-insensitively anywhere in the User-Agent)
   → `subscription`. The client's nature wins over the token shape it carries
   — a Claude Code session uses an `sk-ant-oat*` token but is a subscription
   client.
2. `Authorization: Bearer sk-ant-oat*` → `oauth` (Claude Pro/Max OAuth).
3. `Authorization: Bearer sk-ant-api*` or `Bearer sk-*` → `payg`.
4. `Authorization: Bearer <jwt>` (three dot-separated segments) → `oauth`
   (Codex / Cursor / Copilot OAuth).
5. `Authorization` present but not `Bearer` (e.g. `AWS4-HMAC-SHA256`) → `oauth`.
6. `x-api-key` present → `payg`.
7. `x-goog-api-key` present → `payg`.
8. Default → `payg` (safest default: aggressive compression on a
   misclassified request costs a re-run, not a revoked subscription).

Non-UTF-8 header values never panic; they fall through to the default with a
`tracing::warn!`.

## What each mode changes

### Compression policy (`CompressionPolicy::for_mode`, PR-F2)

| Knob | `payg` | `oauth` | `subscription` |
|---|---|---|---|
| Live-zone-only compression | no | no | **yes** (frozen prefix untouched) |
| CacheAligner | on | on | **off** (no prefix destabilisation) |
| Volatile-token threshold | standard | standard | tighter |
| Max lossy ratio | standard | standard | lower |
| TOIN learning | read-write | read-write | **read-only** |

(`oauth` is currently identical to `payg` in F2.1/F2.2; it exists as a
separate class so telemetry — keyed per mode via the TOIN aggregation key
`(auth_mode, model_family, structure_hash)`, PR-F3 — can justify divergence.)

### Forwarded headers (PR-F4)

`build_forward_request_headers` appends `X-Forwarded-For`,
`X-Forwarded-Proto`, `X-Forwarded-Host`, and `X-Request-Id` for `payg` and
`oauth` traffic, but **skips all synthetic headers for `subscription`**
(fingerprint risk). Client-sent `X-Forwarded-For` values still pass through
unmodified — only Headroom's own appended hop is suppressed. The same gate
applies to the WebSocket proxy path.

### Token handling (PR-F3)

The subscription tracker never stores a raw OAuth bearer in memory: it keeps
only a one-way identifier (`sha256:<16 hex>…<last-4>`) for debugging, and
polls the subscription API with the token read from
`$CLAUDE_CODE_OAUTH_TOKEN` / the credentials file at poll time.

## Staged rollout

All auth-mode-dependent behavior is gated behind
`auth_mode_policy_enforcement` (config; default **disabled**). While
disabled, every request is treated as `payg` — identical to pre-Phase-F
behavior. The classifier still runs and labels logs/metrics in every mode,
so operators can validate classification in production before flipping
enforcement on.
