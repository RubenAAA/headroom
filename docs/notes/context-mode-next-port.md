# context-mode `next` → Rust port doc

Source: `context-mode/` sidecar clone, range `589d821` (v1.0.169) .. `origin/next` `e31360d` (2026-08-12), 10 commits. Checked 2026-09-11. `origin/main` has only `stats.json` churn in this window — all code value is on `next`, unmerged and unreleased (both branches still `1.0.169`, npm still `1.0.169`).

Rust target: `crates/headroom-proxy/src/ctx/fetch.rs` (CTX-5, "Port of context-mode's `ctx_fetch_and_index`"). It predates all of the below: fetch → `htmd::convert` → index whole doc, with only a `trim().is_empty()` guard (`fetch.rs:259`), `User-Agent`-only request (`fetch.rs:214`), no sibling fallback, no template/content split.

## P1 — Shell detection (port first)

From `0ce043f` (`0ce043fe8c3900d4b802181abae184e33aba1263`, "fix(fetch): stop reporting a JavaScript-rendered shell as a successful fetch").

What upstream did: the only emptiness guard was `markdown.length === 0`, so excalidraw (6,862 B in → 21 B "Excalidraw Whiteboard" out) indexed as success. New `classifyExtraction(textBytes, sourceBytes)` refuses as `shell` iff text < 200 B **and** yield < 2%; neither signal alone (ratio alone condemns `<p>Hello</p>`; floor alone condemns short docs). Shell = `fetch_error` with both byte counts, nothing indexed. Subprocess reports pre-conversion bytes (their stdout line 2); missing line reads as "no evidence", never as failure.

Rust change: keep `raw.len()` alongside `markdown`, add the same two-constant check before `index_content`, return the shell error instead of indexing. ~20 lines, no deps, no regex.

Acceptance: excalidraw URL refuses with diagnosis; the three control pages in the commit message (app.diagrams.net, nextjs.org, developers.cloudflare.com) still index.

## P2 — `Accept: text/markdown` on the same request

From `5b9c00c` (`5b9c00c965515445c3d94160a34508a76bbd1d01`, "feat(fetch): extract the article instead of transliterating the page"), rung 1.

What upstream did: send `Accept: text/markdown, text/x-markdown;q=0.9, text/html;q=0.8, application/xhtml+xml;q=0.8, */*;q=0.5` on the request already being made — zero extra round trips, superset of the old request so no site can newly break. Measured: Stripe 1,846,885 B HTML → 11,744 B article; GitBook, Mintlify, Resend, Polygon, nextjs.org, cloudflare dev docs behave the same. Route `markdown` skips extraction entirely (`authored`).

Rust change: add the header to the `reqwest` builder in `fetch_and_index`; branch on response `content-type` containing `text/markdown` to skip any extraction pass. ~5 lines.

## P3 — Rung 2 fallback (`.md` sibling → `llms.txt`)

From `8476db7` (`8476db7970061e7f75c4ece4fee3a774722b9c58`, "feat(fetch): finish the ladder — rung 2 recovers SPA pages browser-free"). Tool-copy update in `e31360d` (`e31360dc860594d9bb70664ccc992c3dda5120d7`).

What upstream did: when rung 1 yields a shell, climb only then (happy path stays one request): 2a tries `.md` siblings of the path, 2b fetches `/llms.txt` and follows it only if it names this page at a URL 2a didn't try. Covers Apple docs (36 B text from 17 kB shell; `.md` sibling has the article) and RN docs (ignores `Accept`). Structural acceptance, not status/shape: accept sibling unless content-type is html or body contains `<!doctype html` / `<html` — because Apple serves `.md` with empty Content-Type + leading HTML comment, and angular.dev 200s a missing `.md` with the SPA shell. Every fetch reports which rung answered.

Rust change: on shell verdict, try the sibling URL candidates then `origin/llms.txt` with the same `isMachineReadable` test; record `rung` + `tried` URLs in `FetchResult` or the error. Medium effort; reuse the existing `ssrf_check` + client for each hop.

Acceptance: Apple `documentation/swiftui/view` and `reactnative.dev/docs/view` recover; `ladderTried` names attempted URLs in refusals.

## P4 — Block-level template/content split

From `5b9c00c` (same commit): new `src/fetch/blocks.ts` (318 lines), `src/fetch/extract.ts` (189), `src/fetch/page-store.ts` (215), wired into `src/server.ts` `indexFetched`.

What upstream did: rule is "chrome repeats across pages of the same host; content doesn't" — block = blank-line/heading split outside fenced code, sha256 of whitespace-collapsed lowercased text; `template` iff that hash was seen on a *different* page of the same host. Store whole doc + all labelled blocks in `fetch-pages.db`; FTS gets `content` blocks only. Cold start: first page of a host admits everything as PROVISIONAL, re-runs when page 2 lands. All-template page = refuse (but store bytes anyway). Invariant `reassemble(splitBlocks(x)) === x` byte-for-byte. No regex, nothing truncated. Upstream measurement that motivates it: link-only-line ratio is 28.3% on Stripe vs 0.3% on Resend — no single threshold separates them, so threshold approaches were abandoned.

Rust change: new `page_store` module (two SQLite tables: `pages`, `page_blocks`, same schema), `blocks` module (split/hash/classify), call it in `fetch_and_index` between conversion and `index_content`; on `refuse`, return shell-type error; on second page of a host, re-index provisional predecessors (their `store.index()` replaces same-label rows — check `CtxStore` supports the same replace semantics). Largest saving, largest work.

Acceptance: second fetch of a host shrinks the first page's index entry; `reassemble(splitBlocks(x)) === x` test over CRLF/fenced-code/unicode samples.

## Method / docs (no code, worth reading)

- `c50c392` (`c50c392f0a0af81647e0ed267d9d88b6335cc125`) + `096f933` (`096f9330ae51c6f95d7b500a92e21201529b8bcc`): `docs/research/fetch-ladder-2026-08-12.md`, `fetch-extraction-2026-08-12.md` + `scripts/measure-fetch-ladder.cjs`, `measure-extraction.mts`, `ladder-targets{,2}.json`. The 36-doc-page corpus and the rung-by-rung method; reusable as Rust test fixtures. Notes two acceptance traps (Apple empty Content-Type; angular soft-404) and what is UNVERIFIED (cookie-less redirect loops on Google devsite hosts).
- `078d1d1` (`078d1d1c755b3e1f9d197f522d3ad5a3705e978f`): CONTRIBUTING rules — truncation banned (store whole first; preview labelled with totals + retrieval path), regex ban, live-client proof over unit-only. `FetchResult { label, chunks, bytes }` already approximates the preview contract; adopt the rest.
- `f0f0e71` (`f0f0e71cd209eac1c194aaedaf3a95185e6bc3f8`): generated-bundle rebuild only, nothing to port.

## Do not port

- `c94e8fc` (`c94e8fc440691b6cd9e6c7f7a6d89e265ef7ebe2`, "fix(forward): suppress platform forwards for 24h after HTTP 402"): reverted by `e1d9448` (`e1d9448050d55f89b5ef9e15f0836d23bb1fe025`) with "billing enforcement belongs to the platform side only; the OSS bridge must stay billing-agnostic". Net-zero by upstream intent.
- Incidental in the same diffs: redirect-limit error reworded from "SSRF blocked: redirect chain exceeded" to neutral "redirect chain exceeded … benign locale loop produces this too". Worth mirroring in Rust's redirect-failure message; not a feature.
