"""Judge replayed next steps: original-vs-compressed, with original-vs-original
as the control. Each judgement re-sends the original conversation (a cache
read after replay.py) with the two candidates appended to the last user
message, in random order.

Usage: judge.py RUN_DIR   (replay.py already judges each pair as it goes)
Adds a "judge" key to each RUN_DIR/<n>.json; skips pairs already judged.
"""
import json, os, random, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from replay import body_of, call, meter, prepare, token  # noqa: E402

PROMPT = """[Evaluation, not part of the task above. Do not call any tools.]
Two candidate next turns for the assistant follow, each written with the full
conversation above. Judge which is the better next step for the user's task:
correct beliefs about what the conversation established, sensible action,
no repeated or wasted work. Equivalent means neither is clearly better.
headroom_retrieve is a real tool: it fetches tool output that was cut short,
so calling it costs a round, not a wrong step.

<candidate_1>
{one}
</candidate_1>

<candidate_2>
{two}
</candidate_2>

Reply with JSON only: {{"better": "1" | "2" | "equivalent", "reason": "<one sentence>"}}"""


def render(resp):
    out = []
    for x in resp.get('content', []):
        if x['type'] == 'text':
            out.append(x['text'])
        elif x['type'] == 'tool_use':
            out.append(f"[tool_use {x['name']}] {json.dumps(x['input'])[:4000]}")
    return '\n'.join(out) or '(empty)'


def ask(orig, first, second, tok):
    b = json.loads(json.dumps(orig))
    last = b['messages'][-1]
    content = last['content']
    if isinstance(content, str):
        content = [{'type': 'text', 'text': content}]
    text = PROMPT.format(one=render(first), two=render(second))
    last['content'] = content + [{'type': 'text', 'text': text}]
    r = call(b, tok)
    verdict = None
    if not any(x['type'] == 'text' and '{' in x['text'] for x in r.get('content', [])):
        # Called a tool or answered in prose: no verdict, keep what it did.
        return None, meter(r), r.get('error') or f"no verdict: {render(r)[:300]}"
    for x in r.get('content', []):
        if x['type'] == 'text' and '{' in x['text']:
            try:
                verdict = json.loads(x['text'][x['text'].index('{'):x['text'].rindex('}') + 1])
            except ValueError:
                pass
    return verdict, meter(r), r.get('error')


def judge(path, tok, rng):
    """Judge one replayed pair in place. replay.py calls this right after the
    replay, while the original's prefix is still cached."""
    d = json.load(open(path))
    if 'judge' in d or any('error' in v for v in d['arms'].values()):
        return
    orig = prepare(body_of(d['pick']['orig']))
    results, spent = {}, 0.0
    for x, y in (('a', 'c'), ('b', 'c'), ('a', 'b')):
        swap = rng.random() < 0.5
        first, second = (y, x) if swap else (x, y)
        verdict, m, err = ask(orig, d['arms'][first], d['arms'][second], tok)
        spent += m
        better = (verdict or {}).get('better')
        winner = {'1': first, '2': second}.get(str(better), 'tie' if better == 'equivalent' else None)
        results[f'{x}{y}'] = {'winner': winner, 'reason': (verdict or {}).get('reason'), 'error': err}
    d['judge'], d['judge_meter_pct'] = results, spent
    json.dump(d, open(path, 'w'))
    print(os.path.basename(path), {k: v['winner'] for k, v in results.items()},
          f'judge {spent:.3f}%', flush=True)


if __name__ == '__main__':
    tok = token()
    rng = random.Random(11)
    for f in sorted(os.listdir(sys.argv[1])):
        judge(os.path.join(sys.argv[1], f), tok, rng)
