# Implemented: SearchCompressor emits source lines verbatim

- **Status:** fixed 2026-08-12 (pinned by digit-integrity tests)
- **Source:** `docs/notes/proxy-experiments-closures.md` (item 16)
- **Summary:** the grep parser read ISO timestamps as `file:line:body` and the
  `u64` line slot ate the zero-padded minute (`:02:` → `:2:`) — a data bug,
  not ranking. `SearchMatch` keeps the raw source line; renderers emit it
  verbatim (prefix strip only); parsed fields still drive scoring/selection.


## Telemetry settlement

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **16 — FIXED, root cause found.** Reproduced offline in
  `tests/integration_digit_integrity.rs`, then fixed. It is
  `search_compressor`, and the mechanism is not a time parser at all:
  `parse_match_line` reads a grep hit as `<path><sep><digits><sep><body>`, and
  an ISO-8601 timestamp fits that shape exactly —
  `2026-08-08T23:02:36.174635Z` parses as path `2026-08-08T23`, line `2`, body
  `36.174635Z`. The renderers then rebuilt the line with
  `format!("{}:{}:{}", file, m.line_number, m.content)`, and because
  `line_number` is a `u64` the minute came back unpadded.

  Every detail of the live observation follows from that and confirms it: only
  the *minute* is damaged (it is the line-number slot), the date survives (it is
  inside the path slot), seconds and microseconds survive (inside the body), the
  hex key survives (no colons), and `22:32:11` survives *because 32 has no
  leading zero to lose*. That last one is the clincher — a generic
  leading-zero stripper could not spare it.

  Fix: `SearchMatch` now carries the source line and the renderers emit it
  verbatim. This compressor selects lines; it does not get to rewrite them.
  Rejecting the parse instead was considered and dropped — unparsed lines are
  *dropped* (`lines_unparsed`), so that would trade corruption for data loss.
  A mis-parse can still group a line oddly, which is a ranking bug; it can no
  longer change a digit, which was a data bug.


## Fix 16

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **16:** every reformat boundary emits `transform_byte_integrity` with the
  transform name, input and output byte lengths, and a short SHA-256 of each
  side. Run the payload through and the first stage whose output hash you do not
  expect is the one that edited it. No tool-result content reaches the log.

  **It is a `debug!`, so at the proxy's `info` level it fires zero times** —
  the same trap that left 1d unprovable. It carries
  `target: "headroom::pipeline"`, so enable just this one:

  ```
  RUST_LOG=info,headroom::pipeline=debug
  ```

  Field expressions are not evaluated when the level is off (checked against
  `tracing` directly, not assumed), so the two hashes cost nothing until asked
  for. Leaving it at `info` would hash every reformat input and output on the
  hot path, and `opt_ms` is currently in the healthy list.


## Item 16 raw

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 16. The proxy silently edits the *content* of tool results

Everything above is about the proxy mis-counting its own work. This one is
different in kind: the proxy changed data that an agent then read and acted
on. It should be fixed before any of the metric items.

**The observation.** At 23:04 I read `cache_recache_observed` records
straight out of `headroom-proxy.log` with a Python one-liner, printing the
`timestamp` field verbatim. Two of the five records came back as:

```
2026-08-08T23:2:36.174635Z  ...  wasted_tokens 1965
2026-08-08T23:4:38.372256Z  ...  wasted_tokens 143871
```

The minute field is not zero-padded. Reading the same two records again by
`request_id` gives the true values:

```
2026-08-08T23:02:36.174635Z 1965 162870 f4993f01a4bc27b6
2026-08-08T23:04:38.372256Z 143871 22032 f4993f01a4bc27b6
```

The file is not corrupt. A scan of the whole log — 142,432 JSON records with
a `timestamp` field — finds **zero** malformed against
`^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z$`. The digit was removed between
the file and the conversation, i.e. inside the proxy.

**What survived tells you what the rule is.** In the same payload:

| Fragment | Contains `0N`? | Altered? |
| --- | --- | --- |
| `2026-08-08` (date) | yes, twice | no |
| `.174635` (microseconds) | no | no |
| `22:32:11` | no | no |
| `23:02:36` → `23:2:36` | yes | **yes** |
| `23:04:38` → `23:4:38` | yes | **yes** |
| `f4993f01a4bc27b6` (hex key) | yes (`01`) | no |
| `143871`, `22032` (counts) | no | no |

So it is not a generic leading-zero stripper — the date and the hex key both
contain `0N` groups and both survived. Only a zero-padded group *in a
colon-delimited time* was re-rendered. That signature is a parse-then-format
round trip: something recognises `HH:MM:SS`, parses the minute as an integer,
and writes it back with `to_string()` / `format!("{}")` instead of
`format!("{:02}")`.

**Where to look first.** `crates/headroom-core/src/transforms/pipeline/
reformats/log_template.rs` is the obvious candidate by name — templating log
lines means splitting them into a shape plus extracted variables, and
re-rendering is exactly where padding is lost. `log_compressor.rs` and
`json_minifier.rs` are the next two. Unconfirmed at the time of writing; a
search was running when this was filed. Whoever picks this up should confirm
the site before changing anything, because the fix is one format specifier
and the risk is fixing the wrong one.

**Why this outranks the metric bugs.** A wrong `tok_saved` misleads whoever
reads the dashboard. A wrong digit inside a tool result is fed to a model as
fact. Timestamps are the visible case because they are checkable against the
source file; the same round trip would silently damage version numbers
(`1.09` → `1.9`), zero-padded IDs, ports, exit codes, hashes rendered with
colons — anything where a padded number carries meaning. Nothing in the
current instrumentation would catch it: the proxy books this as a saving.

**Reproduce it.** Print any text through the proxy containing a zero-padded
time and compare with the source:

```bash
grep -o '"timestamp":"[^"]*"' ~/headroom-proxy.log | grep -E '23:0[0-9]:' | head
```

then read the same lines back through a tool result and diff. If the minute
survives, the responsible transform did not fire for that payload — vary the
size, since compression is size-gated.

**Related.** Worth re-checking the "the report came through truncated"
messages seen during the session against this. They were read at the time as
the model's own summarising, and no evidence links them yet — but a transform
that edits content makes the literal reading worth a second look.

---


## Closure 16

*moved from `docs/notes/proxy-experiments-closures.md`*

### First, because it is not an accounting bug

**16 — closed 2026-08-12: the proxy edited the content of tool results.** The
responsible transform was `SearchCompressor`, not `LogTemplate`. Its permissive
grep parser accepted an ISO timestamp as `file:line_number:content`, parsed the
zero-padded minute into a `u64`, then rendered the selected match from those
parts. `:02:` therefore became `:2:`. The fix keeps the raw source line on
`SearchMatch` and emits it verbatim; grouped output removes only the parsed file
prefix and retains the raw remainder. Parsed fields are still used for scoring,
selection and grouping, so nothing downstream depended on losing the padding.

Before/after is pinned in
`selected_lines_preserve_zero_padded_colon_fields_verbatim`: the old renderer
mutates 2 of 3 selected timestamp lines; the current renderer mutates 0 of 3.
`grouped_output_preserves_zero_padded_line_field` covers the alternate output
layout. `integration_digit_integrity` forces the real Anthropic request path to
run `search_compressor` and requires three policy-selected lines to survive
exactly, including padded times, versions, IDs, exit codes and ports; it cannot
pass by skipping compression or omitting the affected lines.

The release artifact containing the fix (SHA-256
`0e7462052ed939a8f117352b60c3e4c9586820a13482d18710457e38c75c0f03`) was
installed at 2026-08-12 12:52:48Z. Through 14:07:02Z, JSON-parsed log records
show 387 forwarded requests and 335 `search_compressor` applications; all 335
were forwarded and reached `sse stream closed`. This is deployment/exercise
evidence, not a live mutation count: the proxy deliberately does not log
outbound tool-result content, so production logs cannot compare those bytes.
