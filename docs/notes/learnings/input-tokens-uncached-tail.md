# Learning: input_tokens is the uncached tail, not a size proxy

- **Source:** `docs/notes/proxy-experiments-2026-08.md` §9c
- **Claim:** warm-turn `input_tokens=2` on 224–270 KB bodies is the uncached remainder (cache counters carry the rest). Use `bytes_out` or summed cache counters.


## Item 9c

*moved from `docs/notes/proxy-experiments-2026-08.md`*

### 9c. All three incompletes share one signature: `input_tokens=2`

Every `stream_incomplete` on 2026-08-08 evening:

```
21:14:14  rid=095e7892  in=2 out=2  cache_write=66872  cache_read= 22238  blocks=1
21:54:36  rid=3408370b  in=2 out=6  cache_write=  700  cache_read=100346  blocks=1
22:16:33  rid=3bb2eb17  in=2 out=2  cache_write=  872  cache_read= 80875  blocks=0
```

All three: `upstream_status=200`, `stop_reason=""`, bodies of 224-270KB, and
`input_tokens` of exactly **2**. A 234KB request does not have 2 input tokens.

The cache counters explain it and are the important part: 22K-100K of
`cache_read` plus up to 66K of `cache_write` per request. Anthropic reports
cached input separately from `input_tokens`, so 2 is the uncached remainder —
the same accounting quirk already documented at `proxy.rs:4663-4671`, where
`attempted_input_tokens` collapsed to 8,059 against 3.66M of real input.

**These turns are not cheap.** Item 9b used the 2-token figure to suggest the
`stream_incomplete` set might be trivial; it isn't, and the corrected byte
measurement in 9b already showed why. Each carries real cached-token cost that
never reaches the ledger.

`blocks=0` on the third is worth a look on its own — the stream closed having
produced no content block at all, yet returned 200.

**Do not use `input_tokens` as a size or cost proxy anywhere.** On a warm
conversation it measures the uncached tail and nothing else. Use `bytes_out`,
or sum the cache counters.

**Most likely explanation, untested:** client-side cancellation. Claude Code
aborts in-flight requests routinely. That would make the *behaviour* normal
and the *silence* the bug — a cancelled request still burned upstream tokens.

**To investigate:** add an event when a client disconnects or a request is
dropped, with whatever usage is known at that point. Until then, no statement
about proxy cost or savings covers this 12%. Check the 12Z spike separately:
one hour at 33% suggests a condition, not a constant.
