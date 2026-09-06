#!/usr/bin/env python3
"""Price a captured corpus twice: as the client sent it, and as the proxy forwarded it.

    proxy_vs_client.py <corpus_dir>

`cachesim.py compare` scores one build against another. This asks the blunter
question the whole project rests on — is the proxy ahead of doing nothing at
all? — by pairing every turn with its own forwarded body and pricing both.

Raw tokens is the column no pricing hypothesis can argue with: whatever the
proxy adds there it adds under every metering rule. The other three are the
hypotheses still in play. `$api` is Anthropic's published pricing. `fitted`
comes from the 5h-window fit, which refuted raw-token metering (R2=0.133) and
put reads at 0.01-0.06x with a real multiplier on writes. `reads-free` is the
same shape taken to its limit. A change that wins on all four is safe to ship;
one that wins on `$api` alone is a bet on the meter.
"""
import sys

from cachesim import (load_corpus, with_forwarded_bodies, score_corpus, API,
                      Weights, DEFAULT_BYTES_PER_TOKEN as BPT)

RAW = Weights(read=1.0, write_5m=1.0, write_1h=1.0, fresh=1.0)
FITTED = Weights(read=0.03, write_5m=1.25, write_1h=1.60)
READS_FREE = Weights(read=0.0, write_5m=1.25, write_1h=2.0)


def main(path):
    turns = load_corpus(path)
    fwd = with_forwarded_bodies(turns, path)
    kept = {t.request_id for t in fwd}
    cli = [t for t in turns if t.request_id in kept]

    print(f"{len(cli)} paired turns\n")
    print(f'{"arm":<10}{"read":>13}{"c-5m":>11}{"c-1h":>11}{"fresh":>9}'
          f'{"RAW":>13}{"$api":>12}{"fitted":>11}{"reads-free":>12}')
    scored = {}
    for name, arm in (("client", cli), ("proxy", fwd)):
        u, _ = score_corpus(arm, BPT)
        scored[name] = u
        print(f"{name:<10}{u.read_tokens:>13,}{u.write_5m_tokens:>11,}"
              f"{u.write_1h_tokens:>11,}{u.input_tokens:>9,}"
              f"{u.billed_with(RAW):>13,.0f}{u.billed_with(API):>12,.0f}"
              f"{u.billed_with(FITTED):>11,.0f}{u.billed_with(READS_FREE):>12,.0f}")

    uc, uf = scored["client"], scored["proxy"]
    print("\nproxy vs plain Claude Code, by what the meter might be:")
    for label, w in (("raw tokens", RAW), ("api dollars", API),
                     ("fitted", FITTED), ("reads free", READS_FREE)):
        print(f"  {label:<14}{uf.billed_with(w) / max(uc.billed_with(w), 1):>8.4f}x")


if __name__ == "__main__":
    main(sys.argv[1])
