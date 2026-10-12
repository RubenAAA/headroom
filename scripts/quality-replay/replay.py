"""Replay captured turn pairs on Sonnet: the client's original body twice and
the body the proxy forwarded once, straight to Anthropic (not through the
proxy, which would compress the original arm too).

Usage: replay.py PICKS.jsonl OUT_DIR BUDGET_PCT
Writes OUT_DIR/<n>.json per pair with the three responses and their usage,
then judges the pair (judge.py) while its prefix is still cached.
Stops once the estimated spend passes BUDGET_PCT of a 5h window (Sonnet
weights from docs/notes/learnings/subscription-meter-weights.md).
Uses this machine's Claude Code OAuth token and never refreshes it; an
expired token stops the run.
"""
import json, os, random, sys, time, urllib.request, urllib.error

MODEL = 'claude-sonnet-5-5'
MAX_TOKENS = 1024
# 5h-meter % per token, Sonnet fit.
W = {'input_tokens': 1.5e-6, 'cache_read_input_tokens': 0.051e-6,
     'cache_creation_input_tokens': 2.83e-6, 'output_tokens': 9.4e-6}
CREDS = [os.path.expanduser(p) for p in
         ('~/.claude-work/.credentials.json', '~/.claude/.credentials.json')]


def token():
    for p in CREDS:
        if os.path.exists(p):
            o = json.load(open(p))['claudeAiOauth']
            if o['expiresAt'] / 1000 < time.time() + 120:
                sys.exit(f'token in {p} expires soon; let Claude Code refresh it, then rerun')
            return o['accessToken']
    sys.exit('no Claude Code credentials found')


def body_of(path):
    r = json.load(open(path))
    b = r.get('body', r)
    return json.loads(b) if isinstance(b, str) else b


def prepare(b):
    """Same edits on both arms: Sonnet, no thinking (Opus thinking signatures
    do not replay on another model), short non-streamed answer."""
    b = dict(b)
    for k in ('thinking', 'context_management', 'output_config', 'stream'):
        b.pop(k, None)
    b['model'], b['max_tokens'] = MODEL, MAX_TOKENS
    b['thinking'] = {'type': 'between_tools'}  # this model's 'off'
    msgs = []
    for m in b['messages']:
        m = {'role': m['role'], 'content': m.get('content')}
        c = m['content']
        if m.get('role') == 'assistant' and isinstance(c, list):
            c = [x for x in c if x.get('type') not in ('thinking', 'redacted_thinking')]
            if not c:
                c = [{'type': 'text', 'text': '(thinking)'}]
            m = {**m, 'content': c}
        msgs.append(m)
    b['messages'] = msgs
    return b


def call(b, tok):
    req = urllib.request.Request(
        'https://api.anthropic.com/v1/messages', data=json.dumps(b).encode(),
        headers={'authorization': f'Bearer {tok}', 'anthropic-version': '2023-06-01',
                 'anthropic-beta': 'oauth-2025-04-20', 'content-type': 'application/json'})
    for attempt in range(4):
        try:
            with urllib.request.urlopen(req, timeout=300) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            err = e.read().decode(errors='replace')[:500]
            if e.code in (429, 529, 500, 503) and attempt < 3:
                time.sleep(30 * (attempt + 1))
                continue
            return {'error': e.code, 'detail': err}
        except (urllib.error.URLError, TimeoutError) as e:
            if attempt < 3:
                time.sleep(30 * (attempt + 1))
                continue
            return {'error': 'network', 'detail': str(e)}
    return {'error': 'retries'}


# On 2026-10-12 the live 5h meter rose about twice what W predicts for these
# direct calls (about 20 pairs, 1% meter steps, other sessions running).
# Budget on the observed rate.
CALIBRATION = 2.0


def meter(resp):
    u = resp.get('usage') or {}
    return CALIBRATION * sum(u.get(k, 0) * w for k, w in W.items())


def main():
    picks_path, out_dir, budget = sys.argv[1], sys.argv[2], float(sys.argv[3])
    os.makedirs(out_dir, exist_ok=True)
    from judge import judge  # here: judge.py imports this module
    tok = token()
    rng = random.Random(11)
    spent = 0.0
    for i, line in enumerate(open(picks_path)):
        dest = os.path.join(out_dir, f'{i:03d}.json')
        if os.path.exists(dest):
            done = json.load(open(dest))
            spent += done.get('meter_pct', 0) + done.get('judge_meter_pct', 0)
            continue
        if spent >= budget:
            print(f'budget reached at pair {i}: {spent:.2f}%')
            break
        pick = json.loads(line)
        orig, fwd = prepare(body_of(pick['orig'])), prepare(body_of(pick['fwd']))
        # Original, forwarded, original again: the repeat is the noise floor.
        arms = {'a': call(orig, tok), 'c': call(fwd, tok), 'b': call(orig, tok)}
        errs = {k: v for k, v in arms.items() if 'error' in v}
        m = sum(meter(v) for v in arms.values())
        spent += m
        json.dump({'pick': pick, 'arms': arms, 'meter_pct': m}, open(dest, 'w'))
        judge(dest, tok, rng)
        spent += json.load(open(dest)).get('judge_meter_pct', 0)
        print(f'pair {i}: {"ERR " + json.dumps(errs)[:300] if errs else "ok"}  '
              f'{m:.3f}% (total {spent:.2f}%)', flush=True)
        if errs and len(errs) == 3 and i == 0:
            sys.exit('first pair failed on every arm; fix the request before spending more')


if __name__ == '__main__':
    main()
