# Learning: a doubled write ratio was workload shift, not regression

- **Source:** `docs/notes/savings-ideas-2.md` §§1–2 (08-31 → 09-03)
- **Claim:** write:read 0.014 → 0.035 looked like the proxy re-writing cache —
  but growth-per-turn doubled in lockstep (802 → ~2000 tokens) with flat floor
  share (82–93%) and flat output: log-crunching days with big tool results.
  Excess-by-cause flags cover 1–46% on baselines vs 76% on 09-03AM (system
  churn + sidecar + thinking drops, all dated that day).
- **Rule:** decompose write into growth vs excess before blaming the proxy;
  the floor share is the regression signal, not the ratio.


## Detail

*moved from `docs/notes/savings-ideas-2.md`*

## 1. The theory that failed

The aggregate write:read ratio went from 0.013–0.015 (08-31, 09-01/02) to
0.035–0.040 today. The first guess was fan-out: many short subagent
sessions, each paying a big first-turn write against little read.

Not so. Writes by conversation lifetime:

| lifetime (turns) | 08-31 | 09-01/02 | 09-03 AM | 09-03 PM |
|------------------|------:|---------:|---------:|---------:|
| 1                |  2.9% |     4.8% |     3.1% |     5.4% |
| 2–5              |  4.5% |     7.6% |     1.9% |     0.1% |
| 6–20             |  7.5% |    11.5% |    11.6% |    12.1% |
| 21+              | 85.1% |    76.1% |    83.4% |    82.4% |
| 21+ bucket w:r   | 0.013 |    0.015 |    0.036 |    0.031 |

One-turn sessions hold about 5% of writes on every day. The ratio doubled
inside long conversations.

Siblings that share a system+tools prefix do read it from cache: nine
conversations read exactly 23,682 tokens (opus, tools fingerprint
`f96a35acddee`, 36 kB tools + 5 kB system). Their 42–55k writes are each
session's own task prompt (`input_tokens` 1–2 on those turns). Re-paid
prefix across all shared fingerprints today: about 15k tokens, 0.3% of
writes. Five of those nine 23,682 reads were not first turns at all; see
section 4.


## Detail

*moved from `docs/notes/savings-ideas-2.md`*

## 2. What the doubling is

Per consecutive turn in a session, `growth = (read + creation + input)_now −
(read + creation + input)_prev` is how many new prompt tokens entered the
conversation. A perfect cache writes exactly `growth`. `excess =
max(0, write − growth)` is content re-written that was already cached.
Turns with `growth < 0` are set aside for section 3.

| day      | turns | growth p50 | write p50 | floor | excess | excess tokens |
|----------|------:|-----------:|----------:|------:|-------:|--------------:|
| 08-31    |  6124 |        802 |       789 | 85.0% |  15.0% |         1.19M |
| 09-01    |  3511 |        862 |       847 | 92.6% |   7.4% |         0.35M |
| 09-02    | 12061 |        814 |       794 | 86.2% |  13.8% |         2.24M |
| 09-03 AM |  2346 |       1808 |      1998 | 82.0% |  18.0% |         1.26M |
| 09-03 PM |  1545 |       2044 |      2065 | 85.6% |  14.4% |         0.77M |

Growth per turn doubled and write per turn doubled with it; the floor share
is flat. Assistant output moved little (median `output_tokens` 307–315
before, 315–399 today), so the growth is on the input side: tool results.
Today's work was log crunching with large tool output. That is a workload
shift, and the proxy is not re-writing cached content on ordinary turns any
worse than before.

Excess by cause, share of that day's excess (flags overlap):

| flag                | 09-03 AM        | 09-03 PM       |
|---------------------|-----------------|----------------|
| system fp changed   | 464k (37%, n=73)| 104k (14%, 41) |
| sidecar adjacent    | 462k (37%, 267) |  62k (8%, 42)  |
| thinking dropped    | 357k (28%, 62)  |  27k (3%, 14)  |
| rebuild boundary    | 254k (20%, 48)  |  47k (6%, 4)   |
| tail diverged       | 116k (9%, 37)   |  36k (5%, 14)  |
| msg1 collapse       |  76k (6%, 2)    |  —             |
| tools fp changed    |  63k (5%, 17)   |  36k (5%, 5)   |
| flagged / unflagged | 76% / 24%       | 26% / 74%      |

On baseline days the flags cover 1% (08-31), 19% (09-01) and 46% (09-02,
mostly `tail_diverged` 620k and one tools change 310k).


## Detail

*moved from `docs/notes/savings-ideas-2.md`*

## 3. The tail the floor hides

`growth < 0` means the cached total shrank: read fell further than write
rose. That is exactly what a collapse looks like (read drops to the shared
23,682 block, the rest is re-written), so these turns carry the losses.
Their writes as a share of that day's later-turn writes:

| day      | turns | writes | share | attributable inside |
|----------|------:|-------:|------:|---------------------|
| 08-31    |   342 |  581k  |  6.8% | — |
| 09-01    |   189 |  268k  |  5.4% | — |
| 09-02    |   793 | 2.82M  | 14.8% | — |
| 09-03 AM |   254 | 1.06M  | 13.1% | thinking dropped 260k, msg1 collapse 198k, sidecar 75k |
| 09-03 PM |   161 | 1.63M  | 23.3% | tracker lost 410k, msg1 collapse 270k, thinking dropped 146k, system-prompt change 213k, sidecar 17k |

About half of today's tail is client or restart origin. The rest is
proxy-side and fixable: roughly 400k AM and 165k PM.
