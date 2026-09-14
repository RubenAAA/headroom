# Troubleshooting Guide

!!! note "Live implementation: Rust"
    The production proxy is the Rust binary (`crates/headroom-proxy`, launched with `cclaude`). Python paths on this page live in the read-only `upstream-python/` mirror — re-resolve any `headroom/*.py` cite there. Commands, endpoints, and log events below were all verified against the Rust sources; anything that only existed on the Python path is listed under [Retired Python-only entries](#retired-python-only-entries) instead of being left as advice.

!!! warning "Do not set HEADROOM_REQUIRE_RUST_CORE=false"
    That variable is only read by the Python mirror (`upstream-python/headroom/proxy/server.py`) — nothing under `crates/` reads it, so it does nothing for the production proxy, and on the mirror `false` selects the degraded mode without the native core. It is not a fix for anything on this page. If you have it exported from the old native-detector workaround, unset it and diagnose forward on the Rust path below.

Solutions for common Headroom issues. Each entry is symptom → where to look → what good looks like.

---

## The Rust diagnostic path (read this first)

Almost every entry below bottoms out in the same two places. Learn them once:

**The log.** The live proxy appends JSON lines to `~/headroom-proxy.log` (rotations `~/headroom-proxy.log.1` … `.4`, rolled by the launcher — scope any `grep -c` to the current run by finding the latest `headroom-proxy starting` marker first, which carries `pid`, `version`, `binary_len`, and `binary_mtime`).

```bash
tail -f ~/headroom-proxy.log
grep '"event":"turn_cost_ledger"' ~/headroom-proxy.log | tail -5
```

!!! note "Per-port log files are mirror-only"
    Per-port files such as `proxy-8787.log` come from the Python mirror's `proxy_log_path()` (`upstream-python/headroom/paths.py`). The Rust binary does not write them. If someone points you at one, re-resolve to `~/headroom-proxy.log`.

**The endpoints.** The proxy serves these itself (see `GET /healthz`, `GET /cache-health`, `GET /metrics`, `GET /stats` in `crates/headroom-proxy/src/proxy.rs`, plus `GET /debug/inflight` behind the loopback guard):

```bash
curl -s localhost:8787/healthz                  # liveness: {"ok":true,...}
curl -s localhost:8787/cache-health | head -40  # hit rates and recent cache busts
curl -s http://127.0.0.1:8787/debug/inflight    # loopback-only: {"in_flight": N}
```

**What good looks like:** `/healthz` returns `ok: true`, `/cache-health` shows a `recent_hit_rate` near your baseline with `last_event_age_seconds` null or old, and `upstream.verdict` is `healthy`.

---

## Proxy Server Issues

### "Proxy won't start"

**Symptom**: `cclaude` hangs waiting for the proxy, or the proxy exits immediately.

**Where to look**:

```bash
# 1. Is anything listening? (same check restart-headroom.sh uses)
lsof -nP -iTCP:8787 -sTCP:LISTEN -t

# 2. Is it answering?
curl -s localhost:8787/healthz
curl -s localhost:8787/cache-health >/dev/null && echo UP || echo DOWN

# 3. What did the start attempt say? (launcher appends here)
tail -30 ~/headroom-proxy.log
```

A running proxy is **reused as-is** — flags on a later command line are ignored. After changing `~/.headroom-flags.sh`, restart; do not just re-run the launcher:

```bash
pkill -f headroom-proxy
restart-headroom.sh   # swaps in target/release/headroom-proxy, rolls back if it fails to come up
```

The restart script refuses to start when `~/.headroom-flags.sh` is missing (a bare proxy serves traffic and silently costs more), so a missing flags file is itself a diagnosis, not a prompt to hand-write flags.

**What good looks like:** a fresh `headroom-proxy starting` line in the log, `/healthz` returning `{"ok": true, ...}`, and `/cache-health` reachable.

### "Connection refused" when calling proxy

**Symptom**: `curl: (7) Failed to connect to localhost port 8787`

!!! warning "The health path is /healthz, not /health"
    The old page said `curl http://localhost:8787/health`. No `/health` route exists — only `/healthz` (plus `/healthz/upstream`) in `crates/headroom-proxy/src/health.rs`. Probing `/health` 404s even on a healthy proxy.

**Where to look**:

```bash
# 1. Liveness first (note the path)
curl -s localhost:8787/healthz

# 2. Upstream reachability is separate from proxy liveness
curl -s localhost:8787/healthz/upstream

# 3. Wrong port? Check the listener, and start with cclaude (not claude:
#    plain claude bypasses the proxy entirely with no error)
lsof -nP -iTCP:8787 -sTCP:LISTEN -t
ps aux | grep headroom-proxy | grep -v grep
```

**What good looks like:** `/healthz` is `ok: true` while `/healthz/upstream` is 503 — that combination means the proxy is fine and the problem is upstream or the network, not Headroom.

### "Upstream rejects a beta token the client no longer sends"

**Symptom**: The upstream API returns an error referencing a beta feature (`anthropic-beta` header) even though the client is no longer sending that header.

**Cause**: The Rust proxy keeps a per-conversation union of `anthropic-beta` / `openai-beta` tokens (`crates/headroom-proxy/src/cache_stabilization/beta_sticky.rs`, the parity port of the Python `SessionBetaTracker`). If the client sent a token on turn N and drops it on turn N+1, the proxy re-injects it to preserve prefix-cache stability. Once a token is in the union it persists for that conversation. Stopping the token on the client side alone is not sufficient.

Two Rust-era differences from the old Python behavior: sessions are keyed per **conversation** (shared with the cache-drift detector), so parallel conversations never inherit each other's tokens; and the tracker only runs inside the compression interceptor — with compression off the proxy is a strict byte-pipe and startup logs `beta_header_sticky_inactive` to say the `enabled` default is not in effect.

**Where to look**: `beta_header_sticky_inactive` in `~/headroom-proxy.log` tells you the protection was never active (compression off). Otherwise the re-injected token is by design.

**Solution**: `disabled` is a diagnostic operator opt-in that forwards the client value verbatim with no state:

```bash
headroom-proxy --beta-header-sticky disabled ...
# or
export HEADROOM_PROXY_BETA_HEADER_STICKY=disabled
```

(Rust equivalent of the old `HEADROOM_BETA_HEADER_STICKY=disabled`, which only the Python mirror reads.) Restart after changing it — a running proxy keeps the flags it was started with. Alternatively, restarting the proxy process clears the in-memory unions.

**What good looks like:** the upstream error stops, at the known cost of a cache miss on the turn the token set changed. If the error persists with `disabled`, the token is coming from the client on the wire — capture the request bytes rather than blaming the tracker.

---

### "Proxy returns errors for some requests"

**Symptom**: Some requests work, others fail with 502/503.

**Where to look** (the old `headroom proxy --log-file … --log-messages` invocation does not exist on the Rust binary — flags come from the command line / `HEADROOM_PROXY_*` env, listed via `headroom-proxy --help`):

```bash
# 1. Upstream refusing? Outranks any cache theory — check the verdict first
curl -s localhost:8787/cache-health | jq '.upstream'

# 2. The log carries the provider's answer per turn
grep '"event":"sidecar_fallback"' ~/headroom-proxy.log | tail -5
grep '"event":"stream_incomplete"' ~/headroom-proxy.log | tail -5
```

**What good looks like:** `upstream.verdict` is `healthy`; failures correlate with a specific `sidecar_fallback.status` / `error` or with upstream outages, not with every turn.

---

## "Cache keeps dropping" / low hit rate

**Symptom**: The statusline shows `⚠ recache … ago`, or `recent_hit_rate` in `/cache-health` sits below baseline.

**Where to look**: `GET /cache-health` → `last_event` (`event_kind`, `attribution_reason`, `origin`, `scope`, `wasted_tokens`, `cache_creation_input_tokens`) plus `last_event_age_seconds`; the same event is the `cache_recache_observed` WARN/INFO line in the log.

The proxy classifies every rebuild (`RecacheEventKind` in `usage_observer.rs`):

| `event_kind` | Level | Meaning |
|---|---|---|
| `drift` | WARN | Charged prefix change — bytes moved. `attribution_reason` names the evidence (`system`, `tools`, `early_messages`, replay-skip reasons, …). |
| `unexplained` | WARN | Replay was applied and the provider still missed — real waste, cause unknown. |
| `expected` | INFO | Rebuild with no drift to blame. Unattributed, not benign-by-proof. |
| `branch` | INFO | Legitimate inbound-tail build (`inbound_tail_replaced`), `wasted_tokens` 0 — not waste. |

Two traps: `concurrent_turn_in_flight` as an `attribution_reason` is a timing race — true, and not the cause. And `branch` events caching `~NK tok` are supposed to happen; the statusline renders them `ℹ`, not `⚠`.

**What good looks like:** `last_event_age_seconds` old or null, `recent_hit_rate` back at baseline, and the only recent events are `branch` with `wasted_tokens: 0`.

## "The books don't add up" (ledger vs stream reconciliation)

**Symptom**: Token or savings figures disagree with an outside number (Anthropic Console CSV on PAYG, utilization meter on subscription).

**Where to look**: join on `request_id` across three log lines:

- `turn_cost_ledger` (INFO) — the provider's own billed totals, summed over every round the proxy ran: `input_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens` (plus the `cache_write_5m/1h_tokens` split, `-1` where the provider publishes none), `output_tokens` (`-1` where unreported), the hidden-round split (`rounds_input_tokens`, `rounds_cache_read_tokens`), and `compression_mode`.
- `sse stream closed` (INFO message) — what the client actually saw on that turn. It deliberately **excludes** hidden continuation rounds, so it never equals the ledger on a retrieving turn.
- `ccr_continuation_usage` (INFO) — the billed continuation rounds the client never saw. This is the bridge between the two above.

Turns that ended without `message_stop` are `stream_incomplete` (WARN): partial counts, deliberately **not** booked. They are a floor on the shortfall, never a subtraction from it. The `[rid] PERF model=…` line carries the model plus `cache_read` / `cache_write` / `tok_out` for the join.

```bash
python3 contrib/reconcile_books.py --log ~/headroom-proxy.log --date 2026-09-07
python3 contrib/reconcile_books.py --log ~/headroom-proxy.log --date 2026-09-07 \
    --console-csv ~/Downloads/anthropic-usage.csv   # PAYG absolute check
```

**What good looks like:** ledger == stream-close + continuation rounds, every time; the remaining gap is fully covered by the `stream_incomplete` count.

## "A turn skipped compression" (sidecar)

**Symptom**: A turn bypassed compression/offload, or you see beta-related 400s on an otherwise healthy session.

**Where to look**: `sidecar_detected` (INFO: `request_id`, `kind`, `original_messages` → `forwarded_messages`, `model_from` → `model_to`, `routed`) vs `sidecar_fallback` (WARN: `request_id`, `status`, `error`). A fallback hands the turn back to the normal pipeline with the client's original body untouched and leaves no trace in per-conversation state — by design the caller cannot tell "not a sidecar" from "sidecar failed", because the response is the same either way. The sidecar also strips the `context-1m` beta token (`strip_long_context_beta` in `sidecar.rs`); a `context-1m` 400 on the sidecar path means you are behind that fix.

```bash
grep -c '"event":"sidecar_detected"' ~/headroom-proxy.log
grep '"event":"sidecar_fallback"' ~/headroom-proxy.log | tail -5   # read status + error here, only here
```

**What good looks like:** small `forwarded_messages` vs `original_messages` on detected lines, and fallbacks rare with a readable `error` naming the cause (bad status from upstream, rejected field, exhausted retries).

## "First turn wrote a lot of cache"

**Symptom**: Large `cache_creation_input_tokens` on conversation opens, or first-turn write counters climbing faster than new conversations justify.

**Where to look**: `first_turn_write_observed` (INFO) with `attribution_reason` in exactly five values (`observability/recache.rs`): `fresh_session`, `arrived_with_history`, `compaction_restart`, `session_key_drift`, `identical_prompt_fanout`. A cold start writing cache is normal and uncharged. The defect signal is the `contradicts_itself: true` subset — a `fresh_session` that **read** cache (not fresh) or an `arrived_with_history` that read **none** (a live conversation whose key moved with no compaction to explain it). Both are live conversations rebuilding under a new key, previously filed as ordinary first turns.

**What good looks like:** reasons match reality (`fresh_session` with zero read, `arrived_with_history` with nonzero read) and contradictions ≈ 0.

## "High latency"

**Symptom**: Requests take longer than expected.

**Where to look**: the `stage_timings` INFO line — one per request, already joined, so a latency question is a log query:

```bash
grep '"event":"stage_timings"' ~/headroom-proxy.log | tail -3 | jq .
```

Fields: `path`, `request_id`, `session_id`, `total_ms`, `inflight`, `stages`. `inflight` is how many requests were inside the pipeline at that moment (null stage = that stage never ran, not 0 ms). Before rotating or restarting under load, poll the loopback-only drain endpoint — rotating while `in_flight` is nonzero strands the live process's log in an unlinked inode:

```bash
curl -s http://127.0.0.1:8787/debug/inflight   # {"in_flight": N}; rotate only at 0
```

**What good looks like:** `total_ms` dominated by upstream stages, `inflight` low and steady, nulls only on stages that should have been skipped for that path.

---

## Build / toolchain issues

### "CI is red but local is green"

**Symptom**: `cargo clippy` or `cargo fmt` passes locally and fails in CI (or vice versa).

**Cause**: toolchain skew. `rust-toolchain.toml` pins `channel = "1.95.0"` (with `rustfmt` + `clippy`) precisely so a lint added in a newer stable cannot break CI without firing locally. Do not bump it casually.

**Solution**: run the same gate CI runs before every push:

```bash
make ci-precheck
```

That is `cargo fmt --all -- --check` + `cargo clippy --workspace -- -D warnings` + `cargo test --workspace` (plus commitlint). Bump procedure if a new stable is genuinely needed: edit the channel string, `rustup update`, `make ci-precheck`, fix any new lints, commit.

**What good looks like:** `make ci-precheck` green means `git push` will not turn red.

---

## Retired Python-only entries

The following sections from the pre-cutover page were verified to live only in the read-only `upstream-python/` mirror and were removed as live advice. The Rust proxy has no equivalent flag, class, or command:

| Removed entry | Why it is invalid post-cutover | Where it lives now (mirror) |
|---|---|---|
| `HEADROOM_REQUIRE_RUST_CORE=false` for native-detector crashes | Nothing under `crates/` reads it; on the mirror it selects degraded mode | `upstream-python/headroom/proxy/server.py` |
| `pip install "headroom-ai[proxy]"` / `[relevance]` / `[all]`, `hnswlib` C++ build advice | The shipped proxy is a cargo binary, not a wheel; `hnswlib` is an optional mirror dependency | `upstream-python/pyproject.toml` |
| `HeadroomClient` / `HeadroomConfig` / `SmartCrusher` tuning (`min_tokens_to_crush`, `max_items_after_crush`, `skip_compression`, relevance `bm25` tier) | Python SDK transform API; compression on the live path is a Rust interceptor behind proxy flags | `upstream-python/headroom/client.py`, `headroom/transforms/smart_crusher.py` |
| `client.validate_setup()`, `store_url="sqlite://…"`, provider snippets | Python SDK setup surface | `upstream-python/headroom/client.py` |
| `simulate()`, `get_metrics()`, Python `logging` debug, manual `SmartCrusher.apply()` | Python debugging surface; replaced by the log + `/cache-health` + `reconcile_books.py` above | `upstream-python/headroom/` |
| Python `headroom proxy --port/--log-file/--log-messages`, `/health` probe | Rust binary uses `--listen` / `HEADROOM_PROXY_*` env and serves `/healthz` | `crates/headroom-proxy/src/proxy.rs`, `src/health.rs` |
| Python exception table (`ConfigurationError`, `ProviderError`, …) | Mirror SDK error types, not proxy diagnostics | `upstream-python/headroom/` |

The full Python doctor (`upstream-python/headroom/cli/doctor.py`) and the Python savings command still exist in the mirror, but on the Rust side they are the reduced `headroom doctor` (liveness + ledger health against the Rust endpoints) and `headroom savings` (durable-ledger report) in `crates/headroom-proxy/src/bin/headroom_cli.rs`.

---

## Getting Help

1. **Scope the log** to the current run (latest `headroom-proxy starting` marker onward) in `~/headroom-proxy.log` and pull the relevant event lines above.
2. **Check `GET /cache-health`** — `recent_hit_rate`, `last_event` + age, and `upstream.verdict` cover most "is it helping / is it upstream" questions.
3. **Run `headroom doctor`** for liveness and ledger health, `headroom savings` for the durable savings view.
4. **File an issue** at https://github.com/headroom-sdk/headroom/issues

When filing an issue, include:

- Proxy version and build identity (the `version`, `binary_len`, `binary_mtime` fields on the `headroom-proxy starting` line)
- Toolchain (`rust-toolchain.toml` channel) and `make ci-precheck` status, if you changed Rust code
- The flags you started with (`~/.headroom-flags.sh` content, or the `cclaude` command line)
- `/cache-health` output and the matching log excerpt (scoped to the run)
- Minimal reproduction (request shape, not credentials)
