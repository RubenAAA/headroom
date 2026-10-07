# Compressing small tool outputs pays; no re-run jump at the 512-byte floor

- **Status:** measured 2026-10-07; no change made. Answers a peer report that
  short grep output (a 43-line `grep -rl`, a 36-line `grep -n`) came back cut
  to five lines and cost a retrieval or a re-run each time.
- **Window:** proxy logs 2026-10-03 08:58Z to 10-07 16:05Z (2,599 compressed
  blocks, 357 retrieval calls, 53 paid continuation rounds); Claude Code
  transcripts of the last 5 days (10,878 grep/rg outputs of 3+ lines).
- **Floor:** the live zone skips a search-results block under 512 bytes
  (`THRESHOLD_SEARCH_RESULTS`, `transforms/live_zone.rs`), about 140 tokens.
  `--min-tokens-to-crush 200` in the flags file is a no-op, as its comment
  says. Smallest compressed search block in the window: 139 tokens.

## Money: positive at every size

Each block's saving is counted once per request that re-sent it, times 0.1
(cache read). A retrieval call is charged its share of the continuation
rounds its own request paid, at API weights (as in
`offload-loses-at-2000-bytes.md`). Only 13 of 357 calls sat in a request that
paid a round; the rest were answered in place beside other tool calls.

| original tokens | blocks | turns re-sent | saved | retrieval | net |
|---|---|---|---|---|---|
| 0–250 | 248 | 66.2 | 63K | 0 | +63K |
| 250–500 | 598 | 40.5 | 170K | 0 | +170K |
| 500–1000 | 838 | 28.0 | 610K | 85K | +525K |
| 1000–2000 | 525 | 25.6 | 791K | 0 | +791K |
| 2000–5000 | 338 | 23.2 | 933K | 28K | +905K |

Charging every call a full 35K round instead turns every bucket under 2,000
tokens negative. That overcharges by about 25 times and is the wrong model
for this path.

## Re-runs: no jump at the floor

Outputs just under 512 bytes reach the model whole; those just over are
compressed. If compression caused re-runs, the rate would step at 512.

| bytes | n | related search in next 2 calls | same command again |
|---|---|---|---|
| 384–512 | 788 | 54.4% | 0.3% |
| 512–640 | 671 | 54.8% | 0.1% |
| 640–800 | 731 | 54.7% | 0.7% |

**File lists are the soft spot.** `grep -l` outputs: 66% follow-up searches
below 512 bytes (n=480), 71% above (n=1,096). About 2 SE, on small samples,
and bigger lists draw more follow-ups anyway. Keeping 5 of 43 paths is a loss
ranking cannot repair, so exempting path-only lists from the search
compressor is the idea to test if this comes up again. Not done here.

`headroom_retrieve` calls are answered by the proxy and rarely reach client
transcripts, so the "same command" column is mostly exact re-runs.
