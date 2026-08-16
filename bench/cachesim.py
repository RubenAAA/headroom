#!/usr/bin/env python3
"""Offline model of Anthropic prompt caching, for benching the proxy for free.

What gets billed for a request is arithmetic, not a model call: given the exact
bytes and where the `cache_control` breakpoints sit, the split into cache read,
cache creation and fresh input is fully determined. So a corpus of captured
request bodies can be scored without sending anything anywhere, and the same
corpus can be scored twice — once as the client sent it, once as the proxy would
forward it — to say what the proxy is worth.

Two things this is NOT:

- It is not a savings figure. It is a number to compare between two arms that
  saw identical input.
- It is not calibrated out of the box. Anthropic publishes no tokenizer, so
  token counts here come from a bytes-per-token constant. Run `validate` against
  real `turn_cost_ledger` lines before trusting an absolute number; the
  comparison between arms is far more robust than either arm alone, because the
  same estimator error applies to both.

Usage:
    cachesim.py score      <corpus_dir> [--bytes-per-token N]
    cachesim.py validate   <corpus_dir> --log <proxy.log>  # against real billing
    cachesim.py compare    <corpus_dir>                    # both arms, totals
    cachesim.py defects    <corpus_dir> [--top N]          # per-turn attribution
    cachesim.py damage     <corpus_dir> [--top N]          # what we changed
    cachesim.py experiment <corpus_dir> [--base client|forwarded]

See bench/README.md for the workflow. It models cache structure and only that:
a rewrite that damaged the conversation but left a shorter prefix scores here as
an improvement, so `defects` and `damage` are a matched pair and neither is safe
to read alone.
"""
from __future__ import annotations

import argparse
import glob
import hashlib
import json
import os
import re
import statistics
import sys
from dataclasses import dataclass, field, replace

# --- Provider constants -----------------------------------------------------
#
# Published Anthropic prompt-caching behaviour. These are the knobs that decide
# whether a turn is cheap or ruinous, so each one is named rather than inlined.

# Weight of each token class relative to fresh input, which is 1.0 by
# definition. Which set applies depends on what you are paying with, and only
# one of them is documented.
#
# `API` is published: 5m writes 1.25x, 1h writes 2x, reads 0.1x.
#
# `SUBSCRIPTION` is NOT published. Anthropic states no token-denominated budget
# for Pro/Max and does not say how the 5h and 7d windows weight the classes.
# The values here are a hypothesis, and the one that matters is `read`: on the
# API, cache reads do not count toward ITPM rate limits at all. If the
# subscription windows behave the same way, reads are free rather than a tenth,
# and since well over 90% of Claude Code tokens are cache reads that changes
# what is worth optimising. Fit it with `bench/fit_weights.py` before believing
# any absolute subscription number out of this file.
#
# The proxy's own `billed_fresh_equivalents` uses a third set — reads 0.1, ALL
# writes 1.25 — which matches neither. `Usage.billed_flat` reproduces it so the
# two can be compared directly.


@dataclass(frozen=True)
class Weights:
    read: float
    write_5m: float
    write_1h: float
    fresh: float = 1.0
    documented: bool = False


API = Weights(read=0.1, write_5m=1.25, write_1h=2.0, documented=True)
SUBSCRIPTION_HYPOTHESIS = Weights(read=0.0, write_5m=1.25, write_1h=2.0)
PROFILES = {"api": API, "subscription": SUBSCRIPTION_HYPOTHESIS}

# A prefix shorter than this is never cached, so a breakpoint on it does nothing.
# 1024 for Sonnet/Opus; Haiku is 2048 and would need a per-model table.
MIN_CACHEABLE_TOKENS = 1024

# Anthropic accepts at most this many breakpoints per request.
MAX_BREAKPOINTS = 4

TTL_SECONDS = {"5m": 300, "1h": 3600}
DEFAULT_TTL = "5m"

# Fallback bytes-per-token. Deliberately a single global rather than a per-
# content-type table: a table would imply a precision the estimator does not
# have. Calibrated 2026-08-17 against 68 paired turns of real `turn_cost_ledger`
# billing, which put the best fit at 2.90 (it had been guessed at 3.6). Re-run
# `validate` after any change to flattening — it is the only thing standing
# between this file and confident nonsense.
DEFAULT_BYTES_PER_TOKEN = 2.9


# --- Content flattening -----------------------------------------------------


@dataclass
class Segment:
    """One cacheable unit of the request, in wire order."""

    tokens: int
    # TTL requested by the `cache_control` on this segment, or None if this
    # segment does not end a cacheable prefix.
    breakpoint_ttl: str | None
    digest: bytes


def _canon(value) -> bytes:
    """Stable bytes for a JSON value.

    Key order is normalised because the cache key is the content, and two
    serialisations of the same block must not look like different prefixes.
    """
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def _tokens(raw: bytes, bytes_per_token: float) -> int:
    return max(1, round(len(raw) / bytes_per_token))


def _blocks(body: dict):
    """Every cacheable block of a request as `(role, block)`, in wire order.

    Order is fixed by the API — tools, then system, then messages — and it is
    the whole reason a late-arriving tool definition can invalidate everything
    behind it.

    String content is normalised into a text block. The provider tokenises the
    rendered prompt, not the JSON envelope, so `"hello"` and
    `[{"type":"text","text":"hello"}]` are the same prefix to the cache even
    though the bytes differ. Claude Code switches a message between the two
    shapes as its cache marker moves on and off it, so treating them as
    different content makes the modelled hit rate collapse for a reason the
    provider would never have.
    """
    for tool in body.get("tools") or []:
        yield "tools", tool
    system = body.get("system")
    if isinstance(system, str):
        yield "system", {"type": "text", "text": system}
    elif isinstance(system, list):
        for block in system:
            yield "system", block
    for message in body.get("messages") or []:
        role = message.get("role", "")
        content = message.get("content")
        if isinstance(content, list):
            for block in content:
                yield role, block
        elif content is not None:
            yield role, {"type": "text", "text": content}


def flatten(body: dict, bytes_per_token: float) -> list[Segment]:
    """Request body -> the linear segment sequence the cache matches against."""
    segments: list[Segment] = []
    for role, block in _blocks(body):
        ttl = None
        if isinstance(block, dict) and isinstance(block.get("cache_control"), dict):
            ttl = block["cache_control"].get("ttl") or DEFAULT_TTL
            # `cache_control` is a directive about the block, not part of it.
            # Hashing it would mean a marker moving from one message to another
            # invalidated both, which is not how the provider behaves — and it
            # collapses the hit rate on real traffic, where the client moves its
            # tail marker forward every single turn.
            block = {k: v for k, v in block.items() if k != "cache_control"}
        raw = _canon(block)
        # The role rides in the digest but not in the token count: it is a few
        # tokens of rendered scaffolding, and two identical texts under
        # different roles are different prefixes.
        digest = hashlib.blake2b(role.encode() + b"\x00" + raw, digest_size=16).digest()
        segments.append(Segment(_tokens(raw, bytes_per_token), ttl, digest))

    # More breakpoints than the provider accepts is a request it would reject,
    # not one it would silently trim. Flagged rather than fixed.
    marked = [s for s in segments if s.breakpoint_ttl]
    if len(marked) > MAX_BREAKPOINTS:
        for extra in marked[:-MAX_BREAKPOINTS]:
            extra.breakpoint_ttl = None
    return segments


# --- Usage ------------------------------------------------------------------


@dataclass
class Usage:
    input_tokens: int = 0
    read_tokens: int = 0
    write_5m_tokens: int = 0
    write_1h_tokens: int = 0

    @property
    def create_tokens(self) -> int:
        return self.write_5m_tokens + self.write_1h_tokens

    def billed_with(self, weights: Weights) -> float:
        """Fresh-equivalents under a given weighting."""
        return (
            self.input_tokens * weights.fresh
            + self.read_tokens * weights.read
            + self.write_5m_tokens * weights.write_5m
            + self.write_1h_tokens * weights.write_1h
        )

    @property
    def billed(self) -> float:
        """API pricing, the only weighting Anthropic documents."""
        return self.billed_with(API)

    @property
    def billed_flat(self) -> float:
        """The proxy's own definition, which prices every write at 1.25x.

        Kept so the bench can be compared against `turn_cost_ledger` on its own
        terms. Under API pricing with `--force-1h-cache-ttl` live this
        understates cost; under subscription metering nobody knows.
        """
        return self.input_tokens + self.read_tokens * API.read + self.create_tokens * API.write_5m

    def __add__(self, other: "Usage") -> "Usage":
        return Usage(
            self.input_tokens + other.input_tokens,
            self.read_tokens + other.read_tokens,
            self.write_5m_tokens + other.write_5m_tokens,
            self.write_1h_tokens + other.write_1h_tokens,
        )


# --- The cache --------------------------------------------------------------


@dataclass
class CacheSim:
    """Anthropic's prompt cache, as far as billing is concerned.

    An entry is keyed by the digest of everything up to and including a
    breakpoint. That is what makes the cache prefix-structured: change one byte
    of message 3 and every breakpoint after it keys differently, no matter how
    much text they share.
    """

    bytes_per_token: float = DEFAULT_BYTES_PER_TOKEN
    # prefix digest -> unix seconds at which it stops being readable
    entries: dict[bytes, float] = field(default_factory=dict)

    def score(self, body: dict, now: float, scope: str = "") -> Usage:
        segments = flatten(body, self.bytes_per_token)

        # Walk once, accumulating the running prefix digest and token count.
        #
        # A digest is recorded at EVERY segment boundary, not just at the
        # breakpoints, because reading and writing are not symmetric. This
        # turn's breakpoints decide what gets written; a read matches whatever
        # prefix was cached earlier, wherever that boundary happened to fall.
        # Modelling reads as breakpoint-aligned makes the hit rate collapse on
        # real traffic — the client advances its tail marker every turn, so the
        # position it cached last turn is never a marker position this turn.
        # The cache is scoped, so the digest chain has to be seeded rather than
        # started empty. Anthropic holds a separate cache per model and per API
        # key: an identical prefix sent to Sonnet does not read back what Opus
        # wrote, and neither does the same prefix under a different credential.
        # A corpus mixes both — the capture-beta capture spans four models, and an
        # auth swap mid-flight changes the scope of a conversation already in
        # progress — so sharing one namespace invents hits that never happened.
        running = hashlib.blake2b(
            (scope or body.get("model") or "").encode(), digest_size=16)
        cumulative = 0
        boundaries: list[tuple[int, bytes]] = []
        breakpoints: list[tuple[int, bytes, str]] = []
        for seg in segments:
            running.update(seg.digest)
            cumulative += seg.tokens
            digest = running.digest()
            boundaries.append((cumulative, digest))
            if seg.breakpoint_ttl:
                # A prefix under the floor is not cacheable, so a breakpoint on
                # it is inert — not a miss to be repaired, simply nothing.
                if cumulative >= MIN_CACHEABLE_TOKENS:
                    breakpoints.append((cumulative, digest, seg.breakpoint_ttl))
        total = cumulative

        usage = Usage()
        if not breakpoints:
            usage.input_tokens = total
            return usage

        # Longest live cached prefix wins, so search boundaries from the end.
        read_upto = 0
        for boundary, digest in reversed(boundaries):
            if self.entries.get(digest, 0) > now:
                read_upto = boundary
                break
        usage.read_tokens = read_upto

        # Everything between what was read and the last breakpoint gets written,
        # each span at the TTL of the breakpoint that closes it.
        previous = read_upto
        for boundary, _, ttl in breakpoints:
            if boundary <= read_upto:
                continue
            span = boundary - previous
            if ttl == "1h":
                usage.write_1h_tokens += span
            else:
                usage.write_5m_tokens += span
            previous = boundary

        # Past the last breakpoint nothing is cached, so it bills fresh. This is
        # the bucket that a block relocated onto the newest message lands in.
        usage.input_tokens = total - breakpoints[-1][0]

        # Store (or refresh) every breakpoint this request presented. A hit
        # renews the entry, which is why an active conversation holds its prefix
        # well past the nominal TTL.
        for _, digest, ttl in breakpoints:
            self.entries[digest] = now + TTL_SECONDS.get(ttl, TTL_SECONDS[DEFAULT_TTL])
        return usage

    def evict_expired(self, now: float) -> None:
        self.entries = {k: v for k, v in self.entries.items() if v > now}


# --- Corpus -----------------------------------------------------------------


@dataclass
class Turn:
    seq: int
    ts: float
    request_id: str
    session_key: str
    body: dict


def load_corpus(path: str) -> list[Turn]:
    """Read a `HEADROOM_CAPTURE_DIR` corpus, ordered as the proxy saw it.

    Ordering comes from the `seq` field, not the filename: the capture restarts
    its counter every process and the envelope is the only reliable order.
    """
    turns = []
    for name in glob.glob(os.path.join(path, "req-*.json")):
        try:
            with open(name) as handle:
                env = json.load(handle)
        except (OSError, ValueError):
            continue
        body = env.get("body")
        if not isinstance(body, dict) or "messages" not in body:
            continue
        turns.append(
            Turn(
                seq=env.get("seq", 0),
                ts=env.get("ts_ms", 0) / 1000.0,
                request_id=env.get("request_id", ""),
                session_key=env.get("session_key", ""),
                body=body,
            )
        )
    turns.sort(key=lambda t: (t.ts, t.seq))
    return turns


def with_forwarded_bodies(turns: list[Turn], path: str) -> list[Turn]:
    """The same turns, carrying what the proxy sent instead of what it received.

    The capture writes the inbound body into `req-*.json` and the forwarded one
    into `out/<request_id>.json`, so the pair is the A/B: identical traffic,
    identical order, one arm rewritten by the proxy. Turns without a forwarded
    file are dropped rather than passed through, so neither arm gets credit for
    a turn the other never saw.
    """
    out = []
    for turn in turns:
        name = os.path.join(path, "out", f"{turn.request_id}.json")
        try:
            with open(name) as handle:
                body = json.load(handle)
        except (OSError, ValueError):
            continue
        if isinstance(body, dict) and "messages" in body:
            out.append(replace(turn, body=body))
    return out


def score_corpus(turns: list[Turn], bytes_per_token: float) -> tuple[Usage, list[tuple[Turn, Usage]]]:
    sim = CacheSim(bytes_per_token=bytes_per_token)
    total = Usage()
    per_turn = []
    for turn in turns:
        # Model plus credential identify the cache; the conversation part of
        # session_key must NOT be included, or two conversations sharing a
        # prefix would each miss what the other cached.
        auth = turn.session_key.split(":")[1] if ":" in turn.session_key else ""
        usage = sim.score(turn.body, turn.ts,
                          scope=f"{turn.body.get('model', '')}\x00{auth}")
        per_turn.append((turn, usage))
        total = total + usage
    return total, per_turn


# --- Validation -------------------------------------------------------------


def load_ledger(path: str) -> dict[str, dict]:
    """Real per-turn billing from the proxy log, keyed by request id."""
    ledger = {}
    with open(path, errors="replace") as handle:
        for line in handle:
            i = line.find("{")
            if i < 0:
                continue
            try:
                fields = json.loads(line[i:]).get("fields", {})
            except ValueError:
                continue
            if fields.get("event") == "turn_cost_ledger":
                ledger[fields.get("request_id")] = fields
    return ledger


def ledger_usage(real: dict) -> Usage:
    """One ledger line as a `Usage`.

    `cache_creation_input_tokens` is the creation total and the TTL fields are
    its split, so the two should agree. On a handful of turns they do not — the
    split reads far above or far below the total, in one case 149,094 against
    258 — and the total is the trustworthy one: it is a single number the
    provider states, while the split is a nested object read separately and can
    be captured a beat apart on a streamed response. Taking the split at face
    value made the simulator look 24% low on creation when it is within 0.5%.

    So the total sets the size and the split only sets the mix.
    """
    total = real.get("cache_creation_input_tokens") or 0
    m5 = real.get("cache_write_5m_tokens") or 0
    h1 = real.get("cache_write_1h_tokens") or 0
    m5, h1 = max(m5, 0), max(h1, 0)  # -1 means the provider published no split
    split = m5 + h1
    if split:
        share_1h = h1 / split
    else:
        share_1h = 1.0  # the proxy asks for the 1-hour tier
    write_1h = round(total * share_1h)
    return Usage(
        input_tokens=real.get("input_tokens") or 0,
        read_tokens=real.get("cache_read_input_tokens") or 0,
        write_5m_tokens=total - write_1h,
        write_1h_tokens=write_1h,
    )


def validate(turns, path, ledger, bytes_per_token):
    """Compare simulated usage against what the provider actually billed.

    Reports the ratio distribution rather than a mean error, because a single
    prefix rebuild dwarfs a hundred ordinary turns and would own any average.

    Scores the FORWARDED bodies. The ledger is the bill for what the proxy put
    on the wire, so pricing the inbound bodies against it charges the simulator
    for the proxy's whole effect: it read 0.68 on creation that way and reads
    1.00 on the bodies that were actually sent.
    """
    _, per_turn = score_corpus(with_forwarded_bodies(turns, path), bytes_per_token)
    rows = []
    for turn, usage in per_turn:
        real = ledger.get(turn.request_id)
        if not real:
            continue
        rows.append((usage, ledger_usage(real)))
    return rows


def report_validation(rows) -> None:
    if not rows:
        print("no captured turn joins a turn_cost_ledger line — is capture on?")
        print("  enable with HEADROOM_CAPTURE_DIR and let live traffic accumulate")
        return
    print(f"paired turns: {len(rows)}\n")
    print(f"{'bucket':>12} {'simulated':>14} {'actual':>14} {'ratio':>8}")
    for name, get in (
        ("read", lambda u: u.read_tokens),
        ("create", lambda u: u.create_tokens),
        ("fresh input", lambda u: u.input_tokens),
        ("billed", lambda u: u.billed),
    ):
        sim_total = sum(get(s) for s, _ in rows)
        real_total = sum(get(r) for _, r in rows)
        ratio = sim_total / real_total if real_total else float("nan")
        print(f"{name:>12} {sim_total:>14,.0f} {real_total:>14,.0f} {ratio:>8.3f}")

    # The estimator is one constant, so the correction is one number: scale it
    # by however far total simulated tokens sit from the real total.
    sim_tok = sum(s.input_tokens + s.read_tokens + s.create_tokens for s, _ in rows)
    real_tok = sum(r.input_tokens + r.read_tokens + r.create_tokens for _, r in rows)
    if sim_tok and real_tok:
        print(f"\n  suggested --bytes-per-token: "
              f"{DEFAULT_BYTES_PER_TOKEN * sim_tok / real_tok:.2f} "
              f"(current {DEFAULT_BYTES_PER_TOKEN})")

    # Structure matters more than totals. If the simulator puts tokens in the
    # right buckets it can be trusted to compare two arms even when its absolute
    # scale is off; if it does not, calibration cannot rescue it.
    agree = sum(
        1
        for s, r in rows
        if (s.read_tokens > s.create_tokens) == (r.read_tokens > r.create_tokens)
    )
    print(f"  turns where read-vs-create dominance agrees: {agree}/{len(rows)} "
          f"({agree / len(rows):.0%})")
    ratios = [s.billed / r.billed for s, r in rows if r.billed]
    if ratios:
        ratios.sort()
        print(f"  per-turn billed ratio: median {statistics.median(ratios):.2f}, "
              f"p10 {ratios[len(ratios)//10]:.2f}, p90 {ratios[9*len(ratios)//10]:.2f}")


# --- Comparison -------------------------------------------------------------


def report_comparison(turns, path, bytes_per_token, weights) -> int:
    """Price the same traffic twice: as the client sent it, as the proxy sent it."""
    forwarded = with_forwarded_bodies(turns, path)
    if not forwarded:
        print(f"no forwarded bodies under {os.path.join(path, 'out')}", file=sys.stderr)
        return 1
    kept = {t.request_id for t in forwarded}
    client = [t for t in turns if t.request_id in kept]

    arms = {
        "claude code": score_corpus(client, bytes_per_token)[0],
        "through proxy": score_corpus(forwarded, bytes_per_token)[0],
    }
    print(f"turns {len(client)}, sessions {len({t.session_key for t in client})}, "
          f"bytes/token {bytes_per_token}\n")
    print(f"{'':>16} {'claude code':>16} {'through proxy':>16} {'change':>9}")
    for name, get in (
        ("read", lambda u: float(u.read_tokens)),
        ("create", lambda u: float(u.create_tokens)),
        ("fresh input", lambda u: float(u.input_tokens)),
        ("billed", lambda u: u.billed_with(weights)),
    ):
        a, b = get(arms["claude code"]), get(arms["through proxy"])
        change = f"{(b - a) / a:+.1%}" if a else "n/a"
        print(f"{name:>16} {a:>16,.0f} {b:>16,.0f} {change:>9}")
    a = arms["claude code"].billed_with(weights)
    b = arms["through proxy"].billed_with(weights)
    print(f"\n  per turn: {a / len(client):,.0f} -> {b / len(client):,.0f}")
    if b:
        print(f"  the proxy buys {a / b:.2f}x the work per unit of budget")
    return 0


# --- Damage -----------------------------------------------------------------


REMINDER = re.compile(r"<system-reminder>.*?</system-reminder>", re.DOTALL)

# Rewrites the proxy makes on purpose, each identified by a marker it leaves in
# the forwarded body. Anything altered WITHOUT one of these is unexplained, and
# that is the bucket worth waking up for — a deliberate feature announces
# itself, a bug does not. Add a signature here when a feature starts rewriting
# content, so the alarm channel stays quiet unless something is actually wrong.
SIGNATURES = {
    "<!--ctx:injected-->": "ctx injection",
    "<headroom:offloaded": "ctx offload",
    "<headroom:compressed": "compression",
}


def _message_units(body: dict) -> list[tuple[bytes, bytes, int, str]]:
    """(digest, reminder-free digest, bytes, role) per message.

    Three things are normalised away before hashing, because each is transport
    churn that no reader of the conversation would call a change:

    - `cache_control`, which is a directive about a block, not content;
    - string content versus a one-element text block, which the client flips
      between as its marker moves and which replay can flip back;
    - block ordering is NOT normalised — order is content.

    The second digest also drops `<system-reminder>` spans, so a turn where the
    only difference is a reminder the client withdrew can be told apart from one
    where real conversation changed. That distinction is the whole point here:
    the append-only guard is deliberately blind to reminder churn, so this mode
    must be able to say "only reminders moved" instead of flagging every turn.
    """
    units = []
    for message in body.get("messages") or []:
        content = message.get("content")
        if isinstance(content, list):
            blocks = [
                {k: v for k, v in b.items() if k != "cache_control"}
                if isinstance(b, dict) else b
                for b in content
            ]
        elif content is None:
            blocks = []
        else:
            blocks = [{"type": "text", "text": content}]
        role = message.get("role", "")
        raw = _canon({"role": role, "content": blocks})
        bare = REMINDER.sub("", raw.decode("utf-8", "replace")).encode()
        units.append((
            hashlib.blake2b(raw, digest_size=16).digest(),
            hashlib.blake2b(bare, digest_size=16).digest(),
            len(raw),
            role,
        ))
    return units


def report_damage(turns, path, top: int) -> int:
    """What the proxy changed about the conversation itself, not its price.

    `defects` rewards anything that shortens the prefix, which a build that
    quietly deleted history would do beautifully. This is the counterweight: it
    ignores cost entirely and reports content the client sent that we did not
    forward, content we invented, and messages we rewrote.

    It reports, it does not judge. Dropping a pruned tool block or offloading an
    old message is deliberate and shows up here exactly like a bug would — the
    numbers are the input to that judgement, not the judgement.
    """
    import difflib

    forwarded = with_forwarded_bodies(turns, path)
    if not forwarded:
        print(f"no forwarded bodies under {os.path.join(path, 'out')}", file=sys.stderr)
        return 1
    client = {t.request_id: t for t in turns}

    kinds = {"dropped": [0, 0], "injected": [0, 0], "rewritten": [0, 0],
             "reminder churn": [0, 0]}
    tools_dropped, system_delta, worst = {}, 0, []
    clean = 0
    for turn in forwarded:
        origin = client.get(turn.request_id)
        if not origin:
            continue
        before = _message_units(origin.body)
        after = _message_units(turn.body)
        moved = 0
        # Align on the reminder-free digest so reminder churn cannot masquerade
        # as a rewrite, then charge it separately inside the aligned runs.
        for op, i1, i2, j1, j2 in difflib.SequenceMatcher(
                a=[u[1] for u in before], b=[u[1] for u in after],
                autojunk=False).get_opcodes():
            if op == "equal":
                for a, b in zip(before[i1:i2], after[j1:j2]):
                    if a[0] != b[0]:
                        kinds["reminder churn"][0] += 1
                        kinds["reminder churn"][1] += abs(a[2] - b[2])
                continue
            lost = sum(u[2] for u in before[i1:i2])
            got = sum(u[2] for u in after[j1:j2])
            kind = {"delete": "dropped", "insert": "injected"}.get(op, "rewritten")
            # A feature that rewrites content signs its work. Only unsigned
            # changes stay in `rewritten`, which is what makes that row an alarm.
            text = _canon([m.get("content") for m in
                           (turn.body.get("messages") or [])[j1:j2]]).decode("utf-8", "replace")
            for marker, label in SIGNATURES.items():
                if marker in text:
                    kind = label
                    break
            size = abs(lost - got) if op == "replace" else (lost or got)
            kinds.setdefault(kind, [0, 0])
            kinds[kind][0] += (i2 - i1) or (j2 - j1)
            kinds[kind][1] += size
            if kind in ("dropped", "rewritten"):
                moved += size

        names_before = {t.get("name") for t in (origin.body.get("tools") or [])}
        names_after = {t.get("name") for t in (turn.body.get("tools") or [])}
        for name in names_before - names_after:
            tools_dropped[name] = tools_dropped.get(name, 0) + 1
        system_delta += (len(_canon(turn.body.get("system")))
                         - len(_canon(origin.body.get("system"))))
        if moved:
            worst.append((moved, turn.request_id))
        else:
            clean += 1

    total = len(worst) + clean
    if not total:
        print("no paired turns")
        return 1
    print(f"turns {total}, of which {clean} carry no unexplained change "
          f"({clean / total:.0%})\n")
    print(f"  {'kind':<16} {'messages':>10} {'bytes':>14}")
    for kind, (count, size) in sorted(kinds.items(), key=lambda kv: -kv[1][1]):
        flag = "  <-- unexplained" if kind in ("dropped", "rewritten") and count else ""
        print(f"  {kind:<16} {count:>10,} {size:>14,}{flag}")
    print(f"\n  system prompt bytes, net: {system_delta:+,}")
    if tools_dropped:
        print("  tool definitions dropped (turns affected):")
        for name, count in sorted(tools_dropped.items(), key=lambda kv: -kv[1])[:12]:
            print(f"    {name:<44} {count:>6,}")
    if worst:
        worst.sort(reverse=True)
        print(f"\n  most-altered {min(top, len(worst))} turns (unexplained only):")
        for size, rid in worst[:top]:
            print(f"    {size:>12,} bytes  {rid}")
    return 0


def report_defects(turns, path, bytes_per_token, weights, top: int) -> int:
    """Per-turn attribution: which turns the proxy loses, and to what.

    The totals in `compare` say how much; this says where. Every turn is priced
    in both arms and the difference is split across the buckets that produced
    it, so a regression names itself instead of hiding inside an aggregate.

    It sees cache structure and nothing else. A rewrite that damaged the
    conversation but shortened the prefix scores as an improvement here.
    """
    forwarded = with_forwarded_bodies(turns, path)
    if not forwarded:
        print(f"no forwarded bodies under {os.path.join(path, 'out')}", file=sys.stderr)
        return 1
    kept = {t.request_id for t in forwarded}
    client = [t for t in turns if t.request_id in kept]

    by_id = {t.request_id: u for t, u in score_corpus(client, bytes_per_token)[1]}
    pairs = []
    for turn, fwd in score_corpus(forwarded, bytes_per_token)[1]:
        cli = by_id.get(turn.request_id)
        if cli:
            pairs.append((turn, cli, fwd))

    # Attribute each turn's swing to the bucket that moved it. Fresh input is
    # weighted 10x a read, so a small token shift there outweighs a large one
    # anywhere else — the split has to be in billed terms, not token terms.
    def split(cli, fwd):
        return {
            "uncached tail": (fwd.input_tokens - cli.input_tokens) * weights.fresh,
            "prefix not read": (cli.read_tokens - fwd.read_tokens) * weights.read,
            "extra writes": (fwd.write_5m_tokens - cli.write_5m_tokens) * weights.write_5m
                            + (fwd.write_1h_tokens - cli.write_1h_tokens) * weights.write_1h,
        }

    lost, gained, causes = 0.0, 0.0, {}
    worst = []
    for turn, cli, fwd in pairs:
        delta = fwd.billed_with(weights) - cli.billed_with(weights)
        if delta > 0:
            lost += delta
            parts = split(cli, fwd)
            cause = max(parts, key=parts.get)
            causes[cause] = causes.get(cause, 0.0) + delta
            worst.append((delta, cause, turn.request_id))
        else:
            gained += -delta

    print(f"turns {len(pairs)}, of which the proxy loses {sum(1 for d, _, _ in worst)}\n")
    print(f"  saved on winning turns  {gained:>14,.0f}")
    print(f"  lost on losing turns    {lost:>14,.0f}")
    print(f"  net                     {gained - lost:>+14,.0f}")
    if causes:
        print("\n  losses by dominant cause:")
        for cause, amount in sorted(causes.items(), key=lambda kv: -kv[1]):
            print(f"    {cause:<18} {amount:>14,.0f}  {amount / lost:>6.1%}")
    if worst:
        worst.sort(reverse=True)
        print(f"\n  worst {min(top, len(worst))} turns (inspect with "
              f"out/<request_id>.json):")
        for delta, cause, rid in worst[:top]:
            print(f"    {delta:>12,.0f}  {cause:<18} {rid}")
    return 0


def report_experiment(turns, path, bytes_per_token, weights, base: str,
                      names: list[str] | None) -> int:
    """Price every registered strategy against the same corpus.

    Two reference arms are always shown. `claude code` is the traffic untouched,
    which is the only number that says whether a proxy is worth running at all;
    `live proxy` is what the capture actually forwarded, which is the bar a new
    idea has to clear to be worth shipping.
    """
    import strategies

    forwarded = with_forwarded_bodies(turns, path)
    kept = {t.request_id for t in forwarded}
    client = [t for t in turns if t.request_id in kept]
    if not client:
        print(f"no paired turns under {path}", file=sys.stderr)
        return 1
    source = forwarded if base == "forwarded" else client

    rows = [("claude code", score_corpus(client, bytes_per_token)[0]),
            ("live proxy", score_corpus(forwarded, bytes_per_token)[0])]
    for name in names or strategies.REGISTRY:
        arm = [replace(t, body=strategies.apply(name, t.body)) for t in source]
        rows.append((f"{name} (on {base})", score_corpus(arm, bytes_per_token)[0]))

    baseline = rows[0][1].billed_with(weights)
    print(f"turns {len(client)}, base {base}, bytes/token {bytes_per_token}\n")
    print(f"  {'arm':<34} {'billed':>14} {'vs claude code':>15} {'uncached':>9}")
    for name, usage in rows:
        billed = usage.billed_with(weights)
        share = usage.input_tokens / billed if billed else 0
        print(f"  {name:<34} {billed:>14,.0f} "
              f"{(billed - baseline) / baseline:>+14.1%} {share:>9.1%}")
    print("\n  A winner here is a cache-structure result only. Run `damage` on it\n"
          "  before shipping: deleting the conversation also scores well.")
    return 0


# --- CLI --------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("mode", choices=("score", "validate", "compare", "defects", "damage", "experiment"))
    parser.add_argument("corpus")
    parser.add_argument("--log", default=os.path.expanduser("~/headroom-proxy.log"))
    parser.add_argument("--bytes-per-token", type=float, default=DEFAULT_BYTES_PER_TOKEN)
    parser.add_argument("--base", choices=("client", "forwarded"), default="client",
                        help="what `experiment` applies each strategy to")
    parser.add_argument("--strategy", action="append",
                        help="run only these strategies (repeatable)")
    parser.add_argument("--top", type=int, default=10,
                        help="how many worst turns `defects` lists")
    parser.add_argument("--weights", choices=sorted(PROFILES), default="api",
                        help="which weighting to bill under; only `api` is documented")
    args = parser.parse_args()

    turns = load_corpus(args.corpus)
    if not turns:
        print(f"no capture envelopes in {args.corpus}", file=sys.stderr)
        return 1

    if args.mode == "validate":
        report_validation(
            validate(turns, args.corpus, load_ledger(args.log), args.bytes_per_token)
        )
        return 0

    if args.mode == "compare":
        return report_comparison(turns, args.corpus, args.bytes_per_token,
                                 PROFILES[args.weights])

    if args.mode == "experiment":
        return report_experiment(turns, args.corpus, args.bytes_per_token,
                                 PROFILES[args.weights], args.base, args.strategy)

    if args.mode == "damage":
        return report_damage(turns, args.corpus, args.top)

    if args.mode == "defects":
        return report_defects(turns, args.corpus, args.bytes_per_token,
                              PROFILES[args.weights], args.top)

    total, per_turn = score_corpus(turns, args.bytes_per_token)
    sessions = len({t.session_key for t in turns})
    print(f"turns {len(turns)}, sessions {sessions}, "
          f"bytes/token {args.bytes_per_token}")
    print(f"  read          {total.read_tokens:>14,}")
    print(f"  create 5m     {total.write_5m_tokens:>14,}")
    print(f"  create 1h     {total.write_1h_tokens:>14,}")
    print(f"  fresh input   {total.input_tokens:>14,}")
    weights = PROFILES[args.weights]
    billed = total.billed_with(weights)
    print(f"  billed        {billed:>14,.0f}  ({args.weights} weights"
          f"{'' if weights.documented else ', UNVERIFIED — see fit_weights.py'})")
    print(f"  billed_flat   {total.billed_flat:>14,.0f}  (proxy definition, all writes 1.25x)")
    print(f"  per turn      {billed / len(turns):>14,.0f}")
    uncached = total.input_tokens / billed if billed else 0
    print(f"  uncached      {uncached:>13.1%} of the bill")
    return 0


if __name__ == "__main__":
    sys.exit(main())
