# Learning: what the subscription meter charges for

Everyone using this repo is on a subscription; API prices do not apply.
These weights come from the subscription meter itself: the 5h and 7d
utilization Anthropic returns on each response. API prices appear only as
a hypothesis the meter was tested against.

- **Window:** 2026-09-29 08:09Z to 2026-10-12 17:30Z, all five proxy logs.
  54,776 native Anthropic turns (`turn_cost_ledger`), 1,940 meter samples
  (`unified_utilization_sample`: Anthropic's own 5h and 7d utilization,
  1% steps, logged when it moves). Scripts: `scripts/meter-fit/`.
- **Method:** within each 5h window, each first crossing of a new 1% level is
  a segment; the turns between two crossings explain the rise. 979 segments
  over 47 windows. Hypotheses are scored with one free scale; a free
  non-negative fit is checked on held-out windows and by bootstrap.
- **API list prices explain the meter well** (8.4% held-out error per
  window, reads included; dropping reads or counting raw tokens is worse, at
  16%). The free fit does better (4.9% held out) and is stable:

  | 5h meter % per 1M tokens | Opus | Sonnet | Opus if API-shaped (reference) |
  |---|---|---|---|
  | fresh input | 1.7 (0.9-2.7) | 1.5 (0.2-2.2) | 1.5 |
  | 1h cache write | 2.69 (2.33-3.16) | 2.83 (2.63-3.07) | 2.99 |
  | cache read | 0.070 (0.066-0.073) | 0.051 (0.048-0.055) | 0.149 |
  | output | 18.7 (17.3-20.2) | 9.4 (8.7-10.2) | 7.5 |

  (5%-95% bootstrap over windows.) Against API prices, output costs about
  2.5x more and cache reads half as much; Sonnet writes cost as much as
  Opus writes, and Sonnet reads 0.73x. About 0.1-0.2M Opus output tokens
  fill 1% of a 5h window.
- **Where the meter went:** output 39%, cache reads 33%, 1h writes 27%,
  fresh input 1%. API pricing would say reads 62%. So shorter output and
  thinking is the largest lever, and input compression works on about 60%
  of the meter (reads and writes), not on the 1% that is fresh input.
- **Forced 1h TTL rechecked in meter units** (`ttl.py`): of 51,910 turns,
  1,386 came 5-60 min after the previous turn of their conversation. A 5m TTL
  would re-write their 206M read tokens. Cost: +17% of a 5h window per day
  if 5m writes cost 5/8 of 1h writes (the API ratio), +42% if the meter
  prices them the same. 1h stays right, by more than the dollar simulation
  in `ideas/rejected/five-minute-ttl.md` said. The 5m write weight itself
  cannot be fitted: every write here was 1h. Break-even is a 5m write at
  0.37x a 1h write; only below that would 5m win.
- **Limits:** use outside this proxy (claude.ai, other machines) adds rises
  the turns do not explain; output may absorb a per-turn cost the ledger does
  not log; Spark and Codex are not on this meter. The 7d window has only 4
  resets and 94 segments, too few to fit freely (API shape: 1.7% held-out).
  Refit after a plan change.
- **Follow-up:** the savings ledger and statusline still price savings in
  API dollars. For subscription users they should report meter %.
