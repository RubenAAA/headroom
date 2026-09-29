#!/usr/bin/env python3
"""Tell traffic mix from a proxy fault when the statusline c/r or uncached % looks high.

Read-only: scans ~/headroom-proxy.log for a time window and reads
localhost:8787/cache-health. Prints aggregates only, never log lines.

Metrics, exactly as contrib/statusline-cache-perf.sh computes them:
  c/r        sum(cache_creation_input_tokens) / sum(cache_read_input_tokens)
             over turn_cost_ledger events that carry client_request_bytes,
             minus turns the statusline calls cold (no_previous_turn).
  uncached%  sum(input_tokens) / sum(billed_fresh_equivalents), same turns.

Rules (thresholds are flags; every rule prints its numbers):
  MIX-ONLY segments, removed one stage at a time:
    S1 first turns       first_turn_write_observed, no_previous_turn, or the
                         first turn seen in the window (nothing to read yet)
    S2 routed traffic    no client_request_bytes, or a model_route_translate
                         model: Codex/Spark/Zen never touch Anthropic cache
    S3 short sessions    conversations with fewer than --min-turns turns
    S4 growth floor      each turn must write the tokens that are new beyond
                         the largest context that conversation has sent
                         (floor = max(0, size - running max of earlier size),
                         size = read + create + uncached). Only create above
                         the floor is rebuilt ground.
  The steady segment is what is left after S1-S3: Anthropic, continuing,
  in a conversation of at least --min-turns turns.

  PROXY flags (any one fires PROXY or MIXED):
    P1 excess     steady excess c/r (create above floor / read) > --excess-limit
    P2 stabilizer steady recache waste with a proxy-suspect attribution_reason
                  (see PROXY_REASONS) / steady read > --waste-limit
    P3 uncached   steady uncached% > --unc-limit
    P4 definition raw sums disagree with the metric: the 5m+1h write split does
                  not add up to cache_creation on single-round steady turns,
                  hidden continuation-round reads move steady c/r, or
                  /cache-health recent read/write disagrees with the log over
                  the same last N turns (each: relative gap > --tol)

  Verdict:
    OK      no metric above its limit and no flag
    MIX     a metric is above its limit, no flag fires
    PROXY   a flag fires and mix removal explains under --mix-share of the gap
    MIXED   a flag fires and mix removal explains at least --mix-share of it
  Gap is the statusline steady c/r minus --cr-limit; the mix share is how much
  of it S1-S3 remove.

Exit code: 0 for OK or MIX, 1 for PROXY or MIXED, 2 when there is no data.
"""
import argparse
import collections
import datetime
import json
import os
import sys
import urllib.request

DEFAULT_LOG = os.path.expanduser("~/headroom-proxy.log")
HEALTH_URL = "http://127.0.0.1:8787/cache-health"
EVENTS = (
    "turn_cost_ledger",
    "turn_cache_fingerprint",
    "prefix_replay_not_replayed",
    "first_turn_write_observed",
    "cache_recache_observed",
    "model_route_translate",
    "headroom-proxy starting",
)
# Recache reasons that point at our own rewrites. Client-driven reasons
# (tools, system, model, ...) and aftershocks of an already-counted drift are
# not ours to fix.
PROXY_REASONS = {"unexplained_after_replay", "prefix_head_changed", "prefix_content_diverged"}
CRUDE_RECENT = 20


def parse_ts(s):
    try:
        return datetime.datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()
    except (ValueError, AttributeError):
        return None


def line_ts(line):
    try:
        return parse_ts(json.loads(line).get("timestamp"))
    except ValueError:
        return None


def seek_window_start(fh, cutoff):
    """Byte offset of the first line at or after `cutoff`; lines are time ordered."""
    lo, hi = 0, os.fstat(fh.fileno()).st_size
    while hi - lo > 1 << 20:
        mid = (lo + hi) // 2
        fh.seek(mid)
        fh.readline()  # drop the partial line
        ts = None
        while ts is None:
            line = fh.readline()
            if not line:
                break
            ts = line_ts(line)
        if ts is None or ts >= cutoff:
            hi = mid
        else:
            lo = mid
    return lo


def load(path, window_s, now=None):
    """Return (turns, first_events, recache_events, routed_rids, restarts) inside the window."""
    cutoff = (now or datetime.datetime.now(datetime.timezone.utc).timestamp()) - window_s
    models, routed = {}, set()
    ledgers, cold, first, recache, restarts = [], set(), {}, {}, 0
    with open(path, "rb") as fh:
        fh.seek(seek_window_start(fh, cutoff))
        for raw in fh:
            if not any(e.encode() in raw for e in EVENTS):
                continue
            try:
                e = json.loads(raw)
            except ValueError:
                continue
            ts = parse_ts(e.get("timestamp"))
            if ts is None or ts < cutoff:
                continue
            f = e.get("fields", {})
            ev, rid = f.get("event"), f.get("request_id")
            if ev == "turn_cost_ledger":
                ledgers.append((ts, f))
            elif ev == "turn_cache_fingerprint":
                models[rid] = f.get("model")
            elif ev == "prefix_replay_not_replayed" and "no_previous_turn" in str(f.get("reason", "")):
                cold.add(rid)
            elif ev == "first_turn_write_observed":
                first[rid] = f.get("attribution_reason") or "?"
            elif ev == "cache_recache_observed":
                recache[rid] = (f.get("attribution_reason") or "?", f.get("wasted_tokens") or 0)
            elif ev == "model_route_translate":
                routed.add(rid)
                models.setdefault(rid, f.get("model"))
            elif "headroom-proxy starting" in str(f.get("message", "")):
                restarts += 1
    turns = []
    for ts, f in ledgers:
        rid = f.get("request_id")
        turns.append({
            "ts": ts, "rid": rid, "conv": f.get("conversation_key") or "?",
            "model": models.get(rid) or "?",
            "routed": rid in routed or not f.get("client_request_bytes"),
            "create": f.get("cache_creation_input_tokens") or 0,
            "read": f.get("cache_read_input_tokens") or 0,
            "fresh": f.get("input_tokens") or 0,
            "billed": f.get("billed_fresh_equivalents") or 0,
            "w5": f.get("cache_write_5m_tokens") or 0,
            "w1": f.get("cache_write_1h_tokens") or 0,
            # Hidden continuation rounds: read/fresh are billed totals over every
            # round, the 5m/1h split covers the client-visible round only.
            "r_read": f.get("rounds_cache_read_tokens") or 0,
            "r_fresh": f.get("rounds_input_tokens") or 0,
            "cold": rid in cold, "first_reason": first.get(rid),
            "recache": recache.get(rid),
        })
    return turns, restarts


def cr(ts):
    r = sum(t["read"] for t in ts)
    return sum(t["create"] for t in ts) / r if r else 0.0


def unc(ts):
    b = sum(t["billed"] for t in ts)
    return 100.0 * sum(t["fresh"] for t in ts) / b if b else 0.0


def rel_gap(a, b):
    return abs(a - b) / max(abs(a), abs(b), 1)


def analyze(turns, min_turns, health=None):
    """Annotate turns and build the stage table, segments and flag inputs."""
    turns = sorted(turns, key=lambda t: t["ts"])
    by_conv = collections.defaultdict(list)
    for t in turns:
        if not t["routed"]:
            by_conv[t["conv"]].append(t)
    for conv_turns in by_conv.values():
        peak = None
        for i, t in enumerate(conv_turns):
            size = (t["read"] - t["r_read"]) + t["create"] + (t["fresh"] - t["r_fresh"])
            t["window_first"] = i == 0
            t["conv_turns"] = len(conv_turns)
            t["floor"] = 0 if peak is None else max(0, size - peak)
            peak = size if peak is None else max(peak, size)

    anth = [t for t in turns if not t["routed"]]
    routed = [t for t in turns if t["routed"]]
    is_first = lambda t: t["cold"] or t["first_reason"] is not None or t["window_first"]
    # What the statusline shows: every non-cold Anthropic turn.
    statusline = [t for t in anth if not t["cold"]] or anth
    s1 = [t for t in statusline if not is_first(t)]
    s3 = [t for t in s1 if t["conv_turns"] >= min_turns]
    steady = s3
    read = sum(t["read"] for t in steady)
    excess = excess_create(steady)

    stages = [
        ("statusline steady", cr(statusline), unc(statusline), len(statusline)),
        ("- first turns (S1)", cr(s1), unc(s1), len(s1)),
        ("- routed (S2, already outside)", cr(s1), unc(s1), len(s1)),
        ("- short sessions (S3)", cr(s3), unc(s3), len(s3)),
        ("- growth floor (S4), excess only",
         excess / read if read else 0.0, unc(s3), len(s3)),
    ]

    suspect = sum(t["recache"][1] for t in steady if t["recache"] and t["recache"][0] in PROXY_REASONS)
    reasons = collections.Counter()
    for t in steady:
        if t["recache"]:
            reasons[t["recache"][0]] += t["recache"][1]

    # P4: the two definitions of "create" must agree on single-round steady
    # turns. Rounds turns are counted apart: their split is visible-round only.
    single = [t for t in steady if not (t["r_read"] or t["r_fresh"])]
    rounds = [t for t in steady if t["r_read"] or t["r_fresh"]]
    tiered = sum(t["w5"] + t["w1"] for t in single)
    created = sum(t["create"] for t in single)
    visible_read = sum(t["read"] - t["r_read"] for t in steady)
    cr_visible = sum(t["create"] for t in steady) / visible_read if visible_read else 0.0
    health_gap = None
    if health and health.get("samples"):
        last = anth[-int(health["samples"]):]
        health_gap = max(rel_gap(sum(t["read"] for t in last), health.get("recent_cache_read_tokens", 0)),
                         rel_gap(sum(t["create"] for t in last), health.get("recent_cache_write_tokens", 0)))

    return {
        "n_all": len(turns), "n_anth": len(anth), "n_routed": len(routed),
        "stages": stages,
        "crude": cr(anth[-CRUDE_RECENT:]),
        "steady_n": len(steady), "steady_read": read,
        "steady_cr": cr(steady), "steady_floor": sum(t["floor"] for t in steady) / read if read else 0.0,
        "steady_excess": stages[-1][1], "steady_unc": unc(steady),
        "waste": suspect / read if read else 0.0, "reasons": dict(reasons),
        "tier_gap": rel_gap(tiered, created) if created else 0.0,
        "rounds_n": len(rounds), "rounds_read": sum(t["r_read"] for t in rounds),
        "cr_visible": cr_visible,
        "rounds_gap": rel_gap(cr_visible, cr(steady)),
        "health_gap": health_gap,
        "statusline_cr": stages[0][1], "statusline_unc": stages[0][2],
        "segments": segments(turns, anth, routed, is_first, min_turns),
        "top_conv": top_conversations(by_conv),
    }


def excess_create(ts):
    """Create above the growth floor, netted within each conversation.

    Netting per conversation, not per turn: a write lands a turn or two after the
    growth it covers, so a per-turn clip counts that lag as rebuilt ground.
    """
    net = collections.defaultdict(int)
    for t in ts:
        net[t["conv"]] += t["create"] - t["floor"]
    return sum(max(0, v) for v in net.values())


def segments(turns, anth, routed, is_first, min_turns):
    seg = collections.defaultdict(list)
    for t in routed:
        seg[("routed", "-", t["model"])].append(t)
    for t in anth:
        cls = "first" if is_first(t) else ("continuing" if t["conv_turns"] >= min_turns else "short")
        seg[("anthropic", cls, t["model"])].append(t)
    rows = []
    for k, v in seg.items():
        read = sum(t["read"] for t in v)
        rows.append({"key": k, "n": len(v), "create": sum(t["create"] for t in v),
                     "excess": excess_create(v) if k[0] == "anthropic" else 0,
                     "cr": cr(v), "unc": unc(v)})
    return sorted(rows, key=lambda r: -r["create"])


def top_conversations(by_conv, k=5):
    rows = []
    for conv, v in by_conv.items():
        rows.append({"conv": conv[:10], "n": len(v), "create": sum(t["create"] for t in v),
                     "excess": excess_create(v), "cr": cr(v), "unc": unc(v)})
    return sorted(rows, key=lambda r: -r["excess"])[:k]


def decide(a, p):
    """Apply the rules to an analysis dict. Returns (verdict, flags, notes)."""
    flags = []
    if a["steady_n"] and a["steady_excess"] > p.excess_limit:
        flags.append(f"P1 excess c/r {a['steady_excess']:.4f} > {p.excess_limit}")
    if a["waste"] > p.waste_limit:
        flags.append(f"P2 proxy-suspect recache waste/read {a['waste']:.4f} > {p.waste_limit}")
    if a["steady_n"] and a["steady_unc"] > p.unc_limit:
        flags.append(f"P3 steady uncached {a['steady_unc']:.2f}% > {p.unc_limit}%")
    if a["tier_gap"] > p.tol:
        flags.append(f"P4 5m+1h split misses cache_creation by {a['tier_gap']:.0%} on single-round turns")
    if a["rounds_gap"] > p.tol:
        flags.append(f"P4 hidden-round reads move steady c/r {a['rounds_gap']:.0%} "
                     f"({a['cr_visible']:.4f} visible vs {a['steady_cr']:.4f} billed)")
    if a["health_gap"] is not None and a["health_gap"] > p.tol:
        flags.append(f"P4 /cache-health disagrees with the log by {a['health_gap']:.0%}")

    bad_cr = a["statusline_cr"] > p.cr_limit
    bad_unc = a["statusline_unc"] > p.unc_limit
    gap = a["statusline_cr"] - p.cr_limit
    after_mix = a["stages"][3][1]  # c/r once S1-S3 are removed
    mix_share = (a["statusline_cr"] - after_mix) / gap if gap > 0 else 1.0
    mix_share = max(0.0, min(1.0, mix_share))
    if flags:
        verdict = "MIXED" if mix_share >= p.mix_share else "PROXY"
    else:
        verdict = "MIX" if (bad_cr or bad_unc) else "OK"
    return verdict, flags, {"mix_share": mix_share, "bad_cr": bad_cr, "bad_unc": bad_unc}


def report(a, verdict, flags, notes, p, window_s, restarts):
    out = []
    w = out.append
    w(f"window {window_s / 3600:.1f}h, {a['n_all']} turns ({a['n_anth']} anthropic, {a['n_routed']} routed), "
      f"{restarts} restart(s) in window")
    w(f"statusline replica: steady c/r {a['statusline_cr']:.4f} (limit {p.cr_limit}), "
      f"crude(last {CRUDE_RECENT}) {a['crude']:.4f}, uncached {a['statusline_unc']:.2f}% (limit {p.unc_limit}%)")
    w("")
    w(f"{'stage':38s} {'turns':>6s} {'c/r':>8s} {'unc%':>7s}")
    for name, c, u, n in a["stages"]:
        w(f"{name:38s} {n:6d} {c:8.4f} {u:7.2f}")
    w(f"steady segment: {a['steady_n']} turns, growth floor c/r {a['steady_floor']:.4f}, "
      f"excess c/r {a['steady_excess']:.4f}, suspect waste/read {a['waste']:.4f}")
    w(f"hidden-round turns in steady: {a['rounds_n']} (billed reads {a['rounds_read']}); "
      f"c/r on client-visible reads {a['cr_visible']:.4f}; 5m+1h split gap {a['tier_gap']:.1%}")
    if a["reasons"]:
        w("recache waste by reason (steady): " + ", ".join(
            f"{k}={v}" for k, v in sorted(a["reasons"].items(), key=lambda kv: -kv[1])))
    w("")
    w("top segments by create (route/class/model):")
    for r in a["segments"][:6]:
        w(f"  {'/'.join(r['key'])[:44]:44s} n={r['n']:4d} create={r['create']:>9d} "
          f"excess={r['excess']:>8d} c/r={r['cr']:.3f}")
    w("top conversations by excess create:")
    for r in a["top_conv"]:
        w(f"  {r['conv']:10s} n={r['n']:4d} create={r['create']:>9d} excess={r['excess']:>8d} c/r={r['cr']:.3f}")
    w("")
    for f in flags:
        w("FLAG " + f)
    hg = a["health_gap"]
    w("/cache-health vs log (last N turns): " + ("unavailable" if hg is None else f"gap {hg:.1%}"))
    w(f"mix removal (S1-S3) explains {notes['mix_share']:.0%} of the c/r gap")
    w(f"VERDICT {verdict}")
    return "\n".join(out)


def fetch_health():
    try:
        with urllib.request.urlopen(HEALTH_URL, timeout=2) as r:
            return json.load(r)
    except (OSError, ValueError):
        return None


def synthetic(kind):
    """Small synthetic turn tables. Timestamps are increasing; sizes grow by `step`."""
    turns, t0 = [], 1_000_000.0

    def conv(name, n, size0, step, extra_create=0, reason=None, first=True, first_reason=None):
        size = size0
        for i in range(n):
            new = size0 if i == 0 else step
            size = size if i == 0 else size + step
            rebuilt = extra_create if i else 0
            create = new + rebuilt
            turns.append({
                "ts": t0 + len(turns), "rid": f"{name}{i}", "conv": name, "model": "claude-x",
                "routed": False, "create": create, "read": max(0, size - create), "fresh": 2,
                "billed": (size - create) * 0.1 + create * 2 + 2, "w5": 0, "w1": create,
                "cold": i == 0 and first, "r_read": 0, "r_fresh": 0,
                "first_reason": first_reason if i == 0 else None,
                "recache": (reason, rebuilt) if rebuilt and reason else None,
            })

    if kind == "mix":
        # Growth-only writes in one long session, plus cold openers that swamp c/r.
        conv("long", 40, 120_000, 1_500)
        for k in range(6):
            conv(f"cold{k}", 1, 90_000, 0)
    elif kind == "proxy":
        # A long session that re-creates 30k of its prefix on every turn.
        conv("long", 40, 120_000, 1_500, extra_create=30_000, reason="unexplained_after_replay")
    elif kind == "mixed":
        conv("long", 40, 120_000, 1_500, extra_create=30_000, reason="prefix_head_changed")
        # Openers the statusline does not call cold: only first_turn_write_observed saw them.
        for k in range(30):
            conv(f"cold{k}", 1, 200_000, 0, first=False, first_reason="fresh_session")
    return turns


def self_test():
    p = parse_args(["--min-turns", "10"])
    for kind, want in (("mix", "MIX"), ("proxy", "PROXY"), ("mixed", "MIXED")):
        a = analyze(synthetic(kind), p.min_turns)
        got, flags, _ = decide(a, p)
        assert got == want, f"{kind}: got {got}, want {want}\n{report(a, got, flags, _, p, 7200, 0)}"
        print(f"self-test {kind}: {got} ok ({len(flags)} flag(s))")
    print("self-test passed")


def parse_args(argv):
    ap = argparse.ArgumentParser(description="Traffic mix or proxy fault behind a high c/r or uncached %.")
    ap.add_argument("--hours", type=float, default=2.0, help="window, default 2")
    ap.add_argument("--log", default=os.environ.get("HEADROOM_PROXY_LOG", DEFAULT_LOG))
    ap.add_argument("--min-turns", type=int, default=10, help="sessions shorter than this are mix (default 10)")
    ap.add_argument("--cr-limit", type=float, default=0.01, help="statusline green line")
    ap.add_argument("--unc-limit", type=float, default=5.0, help="uncached %% limit")
    ap.add_argument("--excess-limit", type=float, default=0.005, help="steady excess c/r limit (P1)")
    ap.add_argument("--waste-limit", type=float, default=0.003, help="steady suspect waste/read limit (P2)")
    ap.add_argument("--tol", type=float, default=0.10, help="relative gap for definition checks (P4)")
    ap.add_argument("--mix-share", type=float, default=0.25, help="gap share that makes PROXY into MIXED")
    ap.add_argument("--self-test", action="store_true")
    return ap.parse_args(argv)


def main():
    p = parse_args(sys.argv[1:])
    if p.self_test:
        return self_test()
    window_s = p.hours * 3600
    try:
        turns, restarts = load(p.log, window_s)
    except OSError as e:
        print(f"cannot read log: {e}", file=sys.stderr)
        return 2
    if not turns:
        print("no turn_cost_ledger events in the window", file=sys.stderr)
        return 2
    a = analyze(turns, p.min_turns, fetch_health())
    verdict, flags, notes = decide(a, p)
    print(report(a, verdict, flags, notes, p, window_s, restarts))
    return 0 if verdict in ("OK", "MIX") else 1


if __name__ == "__main__":
    sys.exit(main())
