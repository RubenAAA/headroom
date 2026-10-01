# Jev (System One) on OpenCode Zen

Jev is a classification model from TypeSafe AI. It does not write text. You send
a piece of text (the `state`) and a set of typed questions. It returns one
structured answer per question: a yes/no likelihood, a pick from a list, or a
grade on a scale. Zen serves it at its own endpoint, so the proxy's chat-style
`--extra-model-route` aliases cannot reach it and `/model` cannot select it.

Measured 2026-10-01 from the WSL dev box, free variant only. Numbers are scoped
to that window. The docs this is drawn from: `opencode.ai/docs/zen` and
`docs.typesafe.ai` (`api.md`, `primitives.md`, `confidence.md`,
`model-jaggedness/jev-1.13.md`).

## Models and access

| ID | Where | Cost | Auth |
|---|---|---|---|
| `jev-1.13-free` | Zen | free, "temporary" per Zen | none needed |
| `jev-1.13` | Zen | $0.042 per 1M input, output free | `OPENCODE_API_KEY`, funded account |
| `jev-latest` | `api.typesafe.ai/v1/systemone` | TypeSafe pricing | TypeSafe key |

We use only the free Zen variant. The paid ID answered `402 Insufficient account
funds` on this account, so it was not measured.

## Endpoints

```
POST https://opencode.ai/zen/v1/systemone      # Zen
POST https://api.typesafe.ai/v1/systemone      # TypeSafe direct
Content-Type: application/json
Authorization: Bearer <key>                    # omit for jev-1.13-free on Zen
```

**Send a `User-Agent`.** Zen sits behind Cloudflare. Python's default
`Python-urllib` agent gets `403 error code: 1010` before the request reaches
Jev. `curl` is not blocked.

## Request

```json
{
  "model": "jev-1.13-free",
  "state": "My payments have failed for three days and I am losing sales.",
  "questions": {
    "is_urgent": {"type": "noul", "instructions": "Does this need urgent attention?"},
    "team": {"type": "choice", "instructions": "Which team handles this?",
             "criteria": {"billing": "Charges and payments", "shipping": "Delivery problems"}},
    "anger": {"type": "score", "instructions": "How frustrated is the writer?",
              "criteria": ["Calm", "Frustrated", "Very angry"]}
  }
}
```

- `model`, `state`, `questions` are all required.
- `state` may be a string, an object or an array (all three answered 200).
- `questions` is a map. The keys are yours and come back in the reply.
- Every question is judged alone against the same `state`, in parallel.

| Type | Fields | Limits |
|---|---|---|
| `noul` | `instructions`, optional `criteria` with `true`/`false` text | needs `instructions` or `criteria` |
| `choice` | `instructions`, `criteria`: label to description (or `null`) | at most 255 options |
| `score` | `instructions`, `criteria`: ordered list of level descriptions | at most 10 levels |

The docs say a score needs at least 2 levels. The free endpoint accepted 1.
Instructions, options and levels may hold JSON, not only strings
(`primitives/advanced.md`).

## Response

```json
{
  "model": "jev-1.13-free",
  "answers": {
    "is_urgent": {"type": "noul", "noul": 0.96},
    "team": {"type": "choice", "choice": "billing", "confidence": 1,
             "probabilities": {"billing": 1, "shipping": 0}},
    "anger": {"type": "score", "score": 1.1, "confidence": 0.82,
              "legend": {"0": "Calm", "1": "Frustrated", "2": "Very angry"},
              "probabilities": {"0": 0.01, "1": 0.88, "2": 0.11}}
  },
  "usage": {"input_tokens": 368, "output_tokens": 57},
  "cost": "0"
}
```

| Type | Fields |
|---|---|
| `noul` | `noul`: 0 to 1, likelihood of yes. No `confidence`. |
| `choice` | `choice`: label with the highest likelihood. `probabilities`: label to likelihood, sums to 1. `confidence`: 0 to 1. |
| `score` | `score`: a blend, may fall between levels (1.1 above). `legend`: index to level text. `probabilities`: per-level, sums to 1. `confidence`: 0 to 1. |

`confidence` is computed from the shape of `probabilities`: all weight on one
option is 1.0, an even spread is low. Low `choice` confidence usually means no
clear winner; low `score` confidence usually means vague or mixed input. Gate on
bands (continue / extra check / hand to a person) and move the cut-offs with the
cost of being wrong.
## Errors

Seen on the free Zen endpoint. The TypeSafe docs list 401, 422, 429, 529. Zen
adds 400, 402 and 403, and uses 401 and 422 differently.

| Status | Cause | Body |
|---|---|---|
| 400 | bad question type | `{"detail":{"error_type":"api_usage_error","message":"Invalid request."}}` |
| 400 | `noul` with no `instructions`/`criteria` | `{"detail":"Noul question must have criteria or instructions: q"}` |
| 400 | 256 options | `{"detail":"Too many choices. Must have at most 255 choices."}` |
| 400 | 11 levels | `{"detail":"Too many score levels. Must have at most 10 levels."}` |
| 400 | state too large | `{"detail":{"error_type":"max_tokens_exceeded"}}` |
| 401 | missing or unknown `model` | `{"type":"error","error":{"type":"ModelError","message":"Model jev-9.99 is not supported"}}` |
| 401 | invalid API key | `{"type":"error","error":{"type":"AuthError","message":"Invalid API key."}}` |
| 401 | body not valid JSON, or empty | same `ModelError`, with an empty model name |
| 402 | paid ID, no funds | `Upstream request failed: Insufficient account funds` |
| 403 | Cloudflare blocks the User-Agent | `error code: 1010` (not JSON) |
| 422 | missing `state`, missing `questions`, or `questions` empty | `Error from provider (Console): Upstream request failed: Endpoint is unavailable.` |

Two traps. A bad JSON body reports a missing model, not a parse error. A missing
field reports "Endpoint is unavailable", which reads like an outage and is not.
A fake key on the free ID is rejected, so send no `Authorization` header at all
unless you hold a real key. 429 and 529 did not occur in this window.

## Limits

- Options per `choice`: 255. Levels per `score`: 10.
- State: 184 KB passed and 193 KB failed with `max_tokens_exceeded`. The
  reply's `usage.input_tokens` was 32,879 at the largest pass, so the cap is
  about 33k input tokens, counted in tokens and not bytes. Repeated text packs
  at ~5.6 bytes per token; real prose packs tighter, so expect less.
- Questions per request: 1,000 worked (1.0 s). No cap found.
- Rate limits: not published and not hit. The free tier is "temporary".

## Latency

Sequential calls, 10 per row, free Zen endpoint, one box, one afternoon.

| Request | p50 | p95 | Input tokens |
|---|---|---|---|
| 1 question, short state | 850 ms | 1,105 ms | 300 |
| 10 questions, short state | 868 ms | 924 ms | 471 |
| 1 question, 2 KB state | 833 ms | 865 ms | 657 |
| 1 question, 8 KB state | 817 ms | 887 ms | 1,749 |
| 1 question, 32 KB state | 850 ms | 1,051 ms | 6,159 |
| `choice` with 50 options | 817 ms | 1,296 ms | 1,473 |
| 12 calls in parallel | 932 ms wall | | |

Single calls (n=1): 100 KB state 1.1 s, 184 KB state 1.2 s, 50 questions 0.75 s,
200 questions 0.9 s, 1,000 questions 1.0 s. Error replies take 0.5 to 0.8 s.

Reading: a call costs about 0.8 s whatever its size, up to 32 KB and 1,000
questions. Parallel calls do not queue (12 in 0.93 s). Plan for roughly one
second per round trip and put many questions in one call. A tiny request still
counts about 277 input tokens of fixed overhead.

## What it does well and badly

From TypeSafe's jev-1.13 page.

- It answers the question as written. Negations, scope words and unstated
  premises are taken literally. Make each question narrow and combine answers in
  code.
- It cannot count or do arithmetic, and it reads dates as text. Extract parts
  with `choice`, then compute in code.
- Unrelated text in `state` lowers accuracy. Pass only the relevant fields.
- It does not treat input as hostile. Instructions inside `state` can shift an
  answer.
- `noul` and `choice` answers need not agree, and a claim and its denial need
  not sum to 1. Do not reuse a cut-off across types.
- It does not write prose. Give it candidates and let it pick.

## Patterns from TypeSafe's cookbooks

- **Rerank:** one request per (query, candidate) with one `noul`. Their legal
  retrieval test over 30 candidates moved top-10 from 38% to 62% for $0.0645 per
  1,200 scorings (jev-1.12, their data, no latency reported).
- **RAG passage filter:** four questions per passage (topic fit, usefulness,
  contradiction, injection), cut-offs applied in code.
- **Confidence routing:** act on the answer only above a confidence threshold.

## CLI: `headroom classify`

Calls the free Zen endpoint straight from the shell. It does not go through the
proxy and sends no working-directory header. Any session can run it through
Bash.

```bash
headroom classify --state "Site is down, checkout returns 500" \
  --noul "Is this an outage?" --noul "Is this a billing issue?"
# q1: 0.92
# q2: 0.29

echo "$text" | headroom classify --noul "Does this need a human?"   # state on stdin

headroom classify --request req.json --json    # choice/score need a request file; `-` reads stdin
```

- `--noul` repeats; the answers are named `q1`, `q2`, ... and print in the order
  asked. `--request` takes the full JSON body from the API section above.
- `--json` prints the raw reply. The default is one line per answer:
  `name: 0.96`, `team: billing (confidence 1.00)`, `anger: 1.10 (confidence 0.82)`.
- `--model` defaults to `jev-1.13-free`. `--endpoint` or `HEADROOM_JEV_URL`
  overrides the URL. A key (`OPENCODE_API_KEY`) is sent only for a model whose ID
  does not end in `-free`.
- It rejects an empty state or empty `questions` before sending. Zen would
  answer those with the misleading 422 above.
- Exit code 1 on any HTTP error, with the status and body on stderr.

Tests: `cargo test -p headroom-proxy --test integration_cli_classify` (the real
binary against a mock Zen).

## Offline test: could Jev gate recall injection?

Question: the proxy injects a `<session_recall>` block (`ctx/inject.rs`). Would
Jev filter its entries to the ones that help the task?

Data, 2026-10-01: captures in `~/headroom-capture-netvalue` whose working
directory is this repo and that carry a recall block. Captures from other
projects were left out because the text goes to a third party. That leaves
5 sessions and 297 requests.

**The recall block does not depend on the ask.** Across the 5 sessions there
were 4 distinct entries in total, the same 4 in every session. The block header
shows why: its retrieval query is
`<system-reminder> Codebase and user instructions are shown below. Be sure to
adhere to these instructions. IMPORTANT: Th`. `derive_queries` in
`ctx/inject.rs` takes the first 120 characters of the first user message, and
that message opens with the CLAUDE.md `<system-reminder>`. Every session of a
project searches the store for the same boilerplate.

Jev scored each of the 4 entries against 4 asks, two questions per pair
(`helps`: would this entry help with the task; `same_subject`). Values are
`helps / same_subject`.

| Ask | `cat replay-prefixes` | block-count script | `_partial_canon` script | test-file patch |
|---|---|---|---|---|
| another session asks what a capture needs preserved | 0.37 / 0.68 | 0.53 / 0.55 | 0.13 / 0.13 | 0.07 / 0.07 |
| find `provider_partial_of_previous_write` recurrences in the proxy log | 0.18 / 0.39 | 0.21 / 0.26 | 0.13 / 0.22 | 0.05 / 0.07 |
| refresh MCP prevalence on the capture corpus | 0.11 / 0.18 | 0.16 / 0.14 | 0.08 / 0.08 | 0.05 / 0.04 |
| how to use the Jev model here | 0.08 / 0.08 | 0.10 / 0.10 | 0.08 / 0.10 | 0.06 / 0.06 |

My reading of the same 16 pairs: Jev ranked the two replay-prefix entries above
the test-file patch for the capture question, and scored the Jev question low
for all four, which is right. It probably missed one: the `_partial_canon`
script analyses partial prefix writes, which is close to the second ask, and
scored 0.13. Those are my labels, not independent ones, on 16 pairs. They show
Jev separates matched from unrelated entries. They do not give a precision
figure.

Conclusion: a Jev gate would hide the symptom. Four fixed entries are wrong for
most asks because the query is wrong. Fixing the query (use the text the user
typed, after the scaffolding) comes first. A Jev rerank is worth testing only
after that, on candidates that vary with the ask. The query fix shipped the same
day (uncommitted at the time of writing). See
`docs/notes/ideas/implemented/recall-query-is-the-opening-scaffolding.md`.

## Data handling

The Zen page states "Prompts and other inputs are not used for training". It
does not say whether that covers the free variant. Treat `state` as sent to a
third party. The proxy's `--redact-sensitive` covers routed chat paths only, not
this endpoint.
