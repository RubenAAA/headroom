# Idea: whole-body fail-open check on Responses write-back

- **Status:** rejected 2026-09-28 — no trigger case in ten days of logs.
- **Source:** upstream `be00a798` (gateway Responses shape, Sept 2026
  session). Its `apply_view` is positional (view message *i* owns slot
  *i*) and returns the body **unchanged** when the pipeline hands back
  a list that doesn't line up with the slots — compression is an
  optimisation, a mangled transcript is a broken request. Our live
  path (`live_zone_responses.rs`) fails open per block (only the
  failing block reverts); there is no whole-body alignment check
  after write-back.
- **Value:** defense against a class of silent transcript corruption
  (`reasoning.encrypted_content` rewritten, `call_id` pairing
  severed) that presents as valid requests the provider rejects —
  or worse, accepts with the reasoning chain cut.
- **Next:** don't build it speculatively. If a mangled Codex
  transcript ever shows up, check first whether per-block revert
  already contained it; only then add a post-write-back alignment
  assertion (slot count / item identity) that falls back to the
  original body. Close with the incident (or lack of one) recorded.

## Findings 2026-09-28 — no mangled transcript to fix

Searched all five logs (09-18→28) for 4xx answers to Responses bodies the
proxy rewrote:

- Codex (gpt-6): 0 upstream 400s and no `invalid_request_error`.
- Spark: every routed upstream error is Zen-side: 913 connect failures, 119
  `server_error`s, 2 `max_output_tokens` rejections.
- CCR continuations (bodies the proxy builds after a retrieval round): 24 of
  3,660 got a 400, all Spark, 19 on 09-23 and 5 on 09-25, none from 09-26
  to 09-28 over 151 more. Each was contained: the proxy served the fetched
  content instead. The upstream body is not logged, so their cause is
  unknown; they are a continuation path, not compression write-back.

Per the rule above, nothing to build. If a Codex 400 names `call_id` or
`encrypted_content`, re-open.
